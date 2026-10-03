//! Native X11 text output
//!
//! Types text into the focused window through the XTEST extension, giving
//! every character the keyboard layout cannot produce a keycode of its own.
//!
//! Why it is built this way: a character the keymap does not map cannot be
//! sent at all - XTEST presses keycodes, not characters - so it has to be bound
//! to a keycode first. Binding one keycode per character and pressing it
//! straight away loses or reorders characters, because clients resolve key
//! events through their own copy of the keyboard mapping and only refresh it
//! when the server tells them it changed. Measured on a GTK4 entry with a tool
//! that rebinds per character: 11 of 12 Han characters were lost at zero delay,
//! 3 of 12 at 2 ms, and the text arrived intact from 5 ms on. This driver binds
//! the distinct characters of a dictation into as few keymap changes as the
//! mapping's unused keycodes allow (one, for any realistic utterance) and waits
//! once for each change to settle, so the settle cost is paid per transcription
//! instead of per character.
//!
//! Two details make "one change" true rather than aspirational. A
//! `ChangeKeyboardMapping` request writes an unbroken range of keycodes, and
//! an Xorg keymap's unused keycodes are scattered singletons (15 of them,
//! runs of one and two, on the machine this was measured on), so the request
//! spans from the first unused keycode to the last and restates the keysyms of
//! the bound keycodes in between. And each unused keycode carries two
//! characters, one on its unshifted level and one on its shifted, so a keymap
//! with 15 unused keycodes covers 30 distinct characters per change. Only text
//! with more distinct characters than that takes a second change.
//!
//! Requirements: an X server with XTEST (Xorg, XWayland). No external tool, no
//! daemon, and no libX11 at build or run time - the X protocol is spoken
//! directly over the socket.

use super::TextOutput;
use crate::error::OutputError;
use std::collections::HashMap;
use std::thread::sleep;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// The event codes XTEST's `fake_input` expects as its `type` argument.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

/// Keysyms worth naming rather than computing.
const KS_RETURN: u32 = 0xff0d;
const KS_TAB: u32 = 0xff09;
const KS_SHIFT_LEFT: u32 = 0xffe1;
const KS_SHIFT_RIGHT: u32 = 0xffe2;

/// Native X11 text output.
pub struct X11Output {
    /// Delay between key presses in milliseconds
    type_delay_ms: u32,
    /// Delay before typing starts in milliseconds
    pre_type_delay_ms: u32,
    /// How long to wait after changing the keyboard mapping before pressing
    /// the keys that depend on it, and again before restoring it
    keymap_settle_ms: u32,
    /// Whether to send Enter after the text
    auto_submit: bool,
    /// Text appended after the transcription (before auto_submit)
    append_text: Option<String>,
}

/// What to do for one character: which keycode to press, and whether Shift
/// reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stroke {
    keycode: u8,
    shift: bool,
}

/// What the driver needs to know to type one string.
#[derive(Debug, Clone, Copy)]
struct TypeOptions {
    type_delay_ms: u32,
    keymap_settle_ms: u32,
    auto_submit: bool,
}

/// Characters the current keyboard mapping can already produce, and how.
struct Layout {
    by_char: HashMap<char, Stroke>,
}

impl Layout {
    /// Index a server keyboard mapping: level 0 is a key's unshifted keysym,
    /// level 1 its shifted one.
    ///
    /// Higher levels (AltGr) are deliberately ignored. A character reachable
    /// only through AltGr falls through to the generated mapping instead, where
    /// it gets a keycode of its own and needs no modifier at all.
    fn from_keysyms(keysyms: &[u32], per_keycode: usize, min_keycode: u8) -> Self {
        let mut by_char = HashMap::new();
        if per_keycode == 0 {
            return Self { by_char };
        }
        for (index, level) in keysyms.chunks(per_keycode).enumerate() {
            let keycode = min_keycode.wrapping_add(index as u8);
            for (shift, keysym) in [false, true].into_iter().zip(level.iter().take(2)) {
                if let Some(ch) = char_for_keysym(*keysym) {
                    by_char.entry(ch).or_insert(Stroke { keycode, shift });
                }
            }
        }
        Self { by_char }
    }

    fn stroke_for(&self, ch: char) -> Option<Stroke> {
        self.by_char.get(&ch).copied()
    }

    /// The keycode carrying `wanted`, for keys that are pressed as themselves
    /// rather than for the character they type (Shift, Return).
    fn keycode_for_keysym(
        keysyms: &[u32],
        per_keycode: usize,
        min_keycode: u8,
        wanted: u32,
    ) -> Option<u8> {
        if per_keycode == 0 {
            return None;
        }
        keysyms
            .chunks(per_keycode)
            .position(|level| level.contains(&wanted))
            .map(|index| min_keycode.wrapping_add(index as u8))
    }
}

/// Keycodes no level of the current mapping binds to anything, and which are
/// therefore free to point at a generated character mapping.
///
/// Ascending, because one `ChangeKeyboardMapping` request writes an unbroken
/// range and the request this driver sends covers everything from the first to
/// the last of these, restating the keysyms of the bound keycodes it spans.
fn spare_keycodes(keysyms: &[u32], per_keycode: usize, min_keycode: u8) -> Vec<u8> {
    if per_keycode == 0 {
        return Vec::new();
    }
    keysyms
        .chunks(per_keycode)
        .enumerate()
        .filter(|(_, level)| level.iter().all(|keysym| *keysym == 0))
        .map(|(index, _)| min_keycode.wrapping_add(index as u8))
        .collect()
}

/// The keysym that types `ch`, if the X protocol has one.
fn keysym_for(ch: char) -> Option<u32> {
    match ch {
        '\n' | '\r' => Some(KS_RETURN),
        '\t' => Some(KS_TAB),
        // Control characters other than the named ones have no keysym that
        // types them.
        c if (c as u32) < 0x20 => None,
        // Latin-1 keysyms are the code point itself; everything above it lives
        // in the Unicode keysym range of 0x01000000 + code point.
        c if (c as u32) <= 0xff => Some(c as u32),
        c => Some(0x0100_0000 | c as u32),
    }
}

/// The character a keysym types, if it types one.
fn char_for_keysym(keysym: u32) -> Option<char> {
    match keysym {
        0 => None,
        KS_RETURN => Some('\n'),
        KS_TAB => Some('\t'),
        0x20..=0xff => char::from_u32(keysym),
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000),
        _ => None,
    }
}

/// One keycode's generated mapping: the keysym on its unshifted level and the
/// one on its shifted level, 0 for a level this batch left unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Binding {
    keycode: u8,
    unshifted: u32,
    shifted: u32,
}

/// One keymap change plus the keystrokes that depend on it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Batch {
    /// Ascending keycodes and the keysyms to write for them
    bindings: Vec<Binding>,
    /// The characters to press, in the order they appear in the text, with the
    /// layout's own keycodes for characters it can already produce
    strokes: Vec<Stroke>,
}

/// Split `text` into batches, binding every character the layout lacks to a
/// spare keycode of its own.
///
/// Characters the layout already produces are pressed as they are; a character
/// that needs a binding keeps it for the rest of its batch. Each spare keycode
/// carries two characters, the first on its unshifted level and the second on
/// its shifted level, so `levels` (1 or 2) characters fit per keycode before
/// the batch is full and the next batch rebinds the same keycodes. Returns the
/// batches and the number of characters no keycode was left to bind (which the
/// caller reports rather than typing the text partially).
fn plan(text: &str, layout: &Layout, spares: &[u8], levels: usize) -> (Vec<Batch>, usize) {
    let capacity = spares.len() * levels;
    let mut batches: Vec<Batch> = Vec::new();
    let mut batch = Batch::default();
    let mut bound: HashMap<u32, Stroke> = HashMap::new();
    let mut used = 0usize;
    let mut unbound = 0usize;

    for ch in text.chars() {
        if let Some(stroke) = layout.stroke_for(ch) {
            batch.strokes.push(stroke);
            continue;
        }
        let Some(keysym) = keysym_for(ch) else {
            continue;
        };
        if let Some(stroke) = bound.get(&keysym) {
            batch.strokes.push(*stroke);
            continue;
        }
        if capacity == 0 {
            unbound += 1;
            continue;
        }
        if used >= capacity {
            // The batch has bound every spare keycode. Pressing these
            // keycodes again needs a new mapping, and the old one has to have
            // reached the client first, so flush and start over on the same
            // keycodes.
            if !batch.strokes.is_empty() {
                batches.push(std::mem::take(&mut batch));
            }
            bound.clear();
            used = 0;
        }
        let keycode = spares[used / levels];
        let shifted = levels == 2 && used % levels == 1;
        let stroke = Stroke {
            keycode,
            shift: shifted,
        };
        let binding = Binding {
            keycode,
            unshifted: if shifted { 0 } else { keysym },
            shifted: if shifted { keysym } else { 0 },
        };
        match batch.bindings.last_mut() {
            Some(last) if last.keycode == keycode => last.shifted = keysym,
            _ => batch.bindings.push(binding),
        }
        bound.insert(keysym, stroke);
        batch.strokes.push(stroke);
        used += 1;
    }

    if !batch.strokes.is_empty() {
        batches.push(batch);
    }
    (batches, unbound)
}

/// Connect to the display named by `$DISPLAY`, and require XTEST to be there.
fn connect_with_xtest() -> Result<RustConnection, OutputError> {
    let (conn, _screen_num) =
        x11rb::connect(None).map_err(|e| OutputError::X11DisplayUnavailable(e.to_string()))?;
    let present = conn
        .query_extension(b"XTEST")
        .map_err(|e| OutputError::X11DisplayUnavailable(e.to_string()))?
        .reply()
        .map_err(|e| OutputError::X11DisplayUnavailable(e.to_string()))?
        .present;
    if !present {
        return Err(OutputError::X11XtestMissing);
    }
    Ok(conn)
}

fn fake_key(conn: &RustConnection, keycode: u8, press: bool) -> Result<(), OutputError> {
    conn.xtest_fake_input(
        if press { KEY_PRESS } else { KEY_RELEASE },
        keycode,
        x11rb::CURRENT_TIME,
        x11rb::NONE,
        0,
        0,
        0,
    )
    .map_err(|e| OutputError::InjectionFailed(format!("XTEST key event failed: {e}")))?;
    Ok(())
}

/// Press and release one key, holding Shift around it when the character needs
/// the shifted level of its keycode.
fn press_key(conn: &RustConnection, stroke: Stroke, shift: Option<u8>) -> Result<(), OutputError> {
    let shift = if stroke.shift { shift } else { None };
    if let Some(shift) = shift {
        fake_key(conn, shift, true)?;
    }
    fake_key(conn, stroke.keycode, true)?;
    fake_key(conn, stroke.keycode, false)?;
    if let Some(shift) = shift {
        fake_key(conn, shift, false)?;
    }
    Ok(())
}

/// Force a round trip, so everything sent so far has been processed.
fn sync(conn: &RustConnection) -> Result<(), OutputError> {
    conn.flush()
        .map_err(|e| OutputError::InjectionFailed(format!("X11 flush failed: {e}")))?;
    conn.get_input_focus()
        .map_err(|e| OutputError::InjectionFailed(format!("X11 sync failed: {e}")))?
        .reply()
        .map_err(|e| OutputError::InjectionFailed(format!("X11 sync failed: {e}")))?;
    Ok(())
}

/// Fold any X11 error from a key-state query into the output error type.
fn key_state_error<E: std::fmt::Display>(e: E) -> OutputError {
    OutputError::InjectionFailed(format!("X11 key state query failed: {e}"))
}

/// Keycodes currently held down that belong to a modifier, so that a
/// transcription cannot combine with a held hotkey chord (Meta+V, say) into an
/// application shortcut.
fn held_modifier_keycodes(conn: &RustConnection) -> Result<Vec<u8>, OutputError> {
    let pressed = conn
        .query_keymap()
        .map_err(key_state_error)?
        .reply()
        .map_err(key_state_error)?
        .keys;
    let modifier_keycodes = conn
        .get_modifier_mapping()
        .map_err(key_state_error)?
        .reply()
        .map_err(key_state_error)?
        .keycodes;

    let mut held: Vec<u8> = modifier_keycodes
        .into_iter()
        .filter(|keycode| *keycode != 0)
        .filter(|keycode| pressed[(*keycode / 8) as usize] & (1 << (*keycode % 8)) != 0)
        .collect();
    held.sort_unstable();
    held.dedup();
    Ok(held)
}

/// The keycodes one `ChangeKeyboardMapping` request must cover to write
/// `bindings`: the whole span from the first to the last, because the request
/// writes an unbroken range.
fn binding_span(bindings: &[Binding]) -> (u8, u8) {
    (bindings[0].keycode, bindings[bindings.len() - 1].keycode)
}

/// The keysyms the server reported for the keycodes `first` through `last`,
/// inclusive, every level of each.
fn original_keysyms(
    original: &[u32],
    first: u8,
    last: u8,
    per_keycode: usize,
    min_keycode: u8,
) -> Vec<u32> {
    let start = (first - min_keycode) as usize * per_keycode;
    let end = (last - min_keycode) as usize * per_keycode + per_keycode;
    original[start..end].to_vec()
}

/// The keysyms one `ChangeKeyboardMapping` request writes for `bindings`: each
/// bound keycode gets its unshifted keysym on the level an unmodified press
/// resolves to and its shifted keysym on the level Shift resolves to, and the
/// keycodes in between - the span covers bound keycodes too, since the request
/// cannot skip them - keep the keysyms they already had.
fn span_keysyms(
    bindings: &[Binding],
    original: &[u32],
    per_keycode: usize,
    min_keycode: u8,
) -> Vec<u32> {
    let (first, last) = binding_span(bindings);
    let mut keysyms = original_keysyms(original, first, last, per_keycode, min_keycode);
    for binding in bindings {
        let offset = (binding.keycode - first) as usize * per_keycode;
        keysyms[offset] = binding.unshifted;
        if per_keycode >= 2 {
            keysyms[offset + 1] = binding.shifted;
        }
    }
    keysyms
}

/// Bind `batch.bindings` into the server's keyboard mapping.
fn apply_bindings(
    conn: &RustConnection,
    bindings: &[Binding],
    original: &[u32],
    per_keycode: usize,
    min_keycode: u8,
) -> Result<(), OutputError> {
    let (first, last) = binding_span(bindings);
    let keysyms = span_keysyms(bindings, original, per_keycode, min_keycode);
    conn.change_keyboard_mapping(last - first + 1, first, per_keycode as u8, &keysyms)
        .map_err(|e| OutputError::InjectionFailed(format!("X11 keymap change failed: {e}")))?;
    Ok(())
}

/// Put the originally reported keysyms back for every keycode we bound.
fn restore_keymap(
    conn: &RustConnection,
    batches: &[Batch],
    original: &[u32],
    per_keycode: usize,
    min_keycode: u8,
) -> Result<(), OutputError> {
    for batch in batches {
        if batch.bindings.is_empty() {
            continue;
        }
        let (first, last) = binding_span(&batch.bindings);
        let keysyms = original_keysyms(original, first, last, per_keycode, min_keycode);
        conn.change_keyboard_mapping(last - first + 1, first, per_keycode as u8, &keysyms)
            .map_err(|e| OutputError::InjectionFailed(format!("X11 keymap restore failed: {e}")))?;
    }
    Ok(())
}

/// Type `text` into the focused window. Blocking; call it from a blocking task.
fn type_text(text: &str, options: TypeOptions) -> Result<(), OutputError> {
    let conn = connect_with_xtest()?;
    let setup = conn.setup();
    let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);

    let reply = conn
        .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)
        .map_err(|e| OutputError::InjectionFailed(format!("X11 keymap read failed: {e}")))?
        .reply()
        .map_err(|e| OutputError::InjectionFailed(format!("X11 keymap read failed: {e}")))?;
    let per_keycode = reply.keysyms_per_keycode as usize;
    let original = reply.keysyms;

    let layout = Layout::from_keysyms(&original, per_keycode, min_keycode);
    let spares = spare_keycodes(&original, per_keycode, min_keycode);
    // Two characters fit on a spare keycode when the mapping has a shifted
    // level for them to sit on.
    let levels = per_keycode.min(2);
    let (batches, unbound) = plan(text, &layout, &spares, levels);

    if unbound > 0 {
        return Err(OutputError::InjectionFailed(format!(
            "no unused keycodes in the X11 keyboard mapping to bind {unbound} character(s) to"
        )));
    }
    if batches.is_empty() && !options.auto_submit {
        return Ok(());
    }

    let needs_remap = batches.iter().any(|batch| !batch.bindings.is_empty());

    let shift_keycode =
        Layout::keycode_for_keysym(&original, per_keycode, min_keycode, KS_SHIFT_LEFT).or_else(
            || Layout::keycode_for_keysym(&original, per_keycode, min_keycode, KS_SHIFT_RIGHT),
        );

    // Release held modifiers for the duration, then put them back: a
    // transcription typed while the hotkey chord is still down would otherwise
    // arrive as application shortcuts.
    let held = held_modifier_keycodes(&conn)?;
    for keycode in &held {
        fake_key(&conn, *keycode, false)?;
    }

    let settle = Duration::from_millis(options.keymap_settle_ms as u64);
    let pace = Duration::from_millis(options.type_delay_ms as u64);
    let last_batch = batches.len().saturating_sub(1);

    for (index, batch) in batches.iter().enumerate() {
        if !batch.bindings.is_empty() {
            apply_bindings(&conn, &batch.bindings, &original, per_keycode, min_keycode)?;
            sync(&conn)?;
            // Clients resolve keycodes through their own copy of the keyboard
            // mapping, refreshed on a keymap-change notification. Pressing a
            // remapped keycode before they have caught up loses or swaps the
            // character.
            sleep(settle);
        }
        for stroke in &batch.strokes {
            press_key(&conn, *stroke, shift_keycode)?;
            if options.type_delay_ms > 0 {
                sleep(pace);
            }
        }
        sync(&conn)?;
        if index < last_batch {
            // The next batch rebinds these keycodes; let this batch's keys be
            // resolved against the mapping they were pressed with.
            sleep(settle);
        }
    }

    if needs_remap && !batches.is_empty() {
        // Wait for the last keys to be resolved before taking their keycodes
        // away again.
        sleep(settle);
        restore_keymap(&conn, &batches, &original, per_keycode, min_keycode)?;
        sync(&conn)?;
        sleep(settle);
    }

    for keycode in held.iter().rev() {
        fake_key(&conn, *keycode, true)?;
    }

    if options.auto_submit {
        if let Some(return_key) =
            Layout::keycode_for_keysym(&original, per_keycode, min_keycode, KS_RETURN)
        {
            press_key(
                &conn,
                Stroke {
                    keycode: return_key,
                    shift: false,
                },
                shift_keycode,
            )?;
        }
    }

    sync(&conn)?;
    Ok(())
}

impl X11Output {
    /// Create a new native X11 output
    pub fn new(
        type_delay_ms: u32,
        pre_type_delay_ms: u32,
        keymap_settle_ms: u32,
        auto_submit: bool,
        append_text: Option<String>,
    ) -> Self {
        Self {
            type_delay_ms,
            pre_type_delay_ms,
            keymap_settle_ms,
            auto_submit,
            append_text,
        }
    }

    async fn type_text_async(&self, text: &str) -> Result<(), OutputError> {
        let options = TypeOptions {
            type_delay_ms: self.type_delay_ms,
            keymap_settle_ms: self.keymap_settle_ms,
            auto_submit: self.auto_submit,
        };
        let payload = text.to_string();
        tokio::task::spawn_blocking(move || type_text(&payload, options))
            .await
            .map_err(|e| OutputError::InjectionFailed(format!("X11 typing task failed: {e}")))?
    }
}

#[async_trait::async_trait]
impl TextOutput for X11Output {
    async fn output(&self, text: &str) -> Result<(), OutputError> {
        if text.is_empty() && self.append_text.is_none() && !self.auto_submit {
            return Ok(());
        }

        if self.pre_type_delay_ms > 0 {
            tracing::debug!("x11: sleeping {}ms before typing", self.pre_type_delay_ms);
            tokio::time::sleep(Duration::from_millis(self.pre_type_delay_ms as u64)).await;
        }

        let mut payload = text.to_string();
        if let Some(append) = &self.append_text {
            payload.push_str(append);
        }

        self.type_text_async(&payload).await?;

        tracing::info!("Text typed via x11 ({} chars)", payload.chars().count());
        Ok(())
    }

    async fn is_available(&self) -> bool {
        tokio::task::spawn_blocking(|| connect_with_xtest().is_ok())
            .await
            .unwrap_or(false)
    }

    fn name(&self) -> &'static str {
        "x11"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_and_unicode_keysyms() {
        assert_eq!(keysym_for('a'), Some(0x61));
        assert_eq!(keysym_for(' '), Some(0x20));
        assert_eq!(keysym_for('é'), Some(0xe9));
        assert_eq!(keysym_for('中'), Some(0x0100_4e2d));
        assert_eq!(keysym_for('🦜'), Some(0x0101_f99c));
        assert_eq!(keysym_for('\n'), Some(KS_RETURN));
        assert_eq!(keysym_for('\t'), Some(KS_TAB));
        assert_eq!(keysym_for('\u{7}'), None);
    }

    #[test]
    fn keysyms_round_trip_to_characters() {
        for ch in ['a', 'é', '中', '🦜'] {
            assert_eq!(char_for_keysym(keysym_for(ch).unwrap()), Some(ch));
        }
        assert_eq!(char_for_keysym(0), None);
        assert_eq!(char_for_keysym(0xff0d), Some('\n'));
        assert_eq!(char_for_keysym(KS_SHIFT_LEFT), None);
    }

    #[test]
    fn layout_indexes_unshifted_and_shifted_levels() {
        let layout = Layout::from_keysyms(&[0x61, 0x41, 0, 0], 2, 8);
        assert_eq!(
            layout.stroke_for('a'),
            Some(Stroke {
                keycode: 8,
                shift: false
            })
        );
        assert_eq!(
            layout.stroke_for('A'),
            Some(Stroke {
                keycode: 8,
                shift: true
            })
        );
        assert_eq!(layout.stroke_for('中'), None);
    }

    #[test]
    fn layout_finds_named_keycodes() {
        // keycodes 8 = a, 9 = Shift_L
        let keysyms = [0x61, 0x41, 0xffe1, 0, 0, 0];
        assert_eq!(
            Layout::keycode_for_keysym(&keysyms, 2, 8, KS_SHIFT_LEFT),
            Some(9)
        );
        assert_eq!(Layout::keycode_for_keysym(&keysyms, 2, 8, KS_RETURN), None);
    }

    #[test]
    fn spare_keycodes_lists_unbound_keycodes_ascending() {
        // per_keycode 2, min 8: keycodes 8 bound, 9 free, 10 bound, 11 free
        let keysyms = [0x61, 0, 0, 0, 0x62, 0, 0, 0];
        assert_eq!(spare_keycodes(&keysyms, 2, 8), vec![9, 11]);
        assert_eq!(spare_keycodes(&[], 0, 8), Vec::<u8>::new());
    }

    /// The stroke a keycode is pressed for, so the tests read as text.
    fn stroke(keycode: u8, shift: bool) -> Stroke {
        Stroke { keycode, shift }
    }

    #[test]
    fn plan_reuses_layout_characters_without_binding() {
        let layout = Layout::from_keysyms(&[0x61, 0, 0, 0], 2, 8);
        let (batches, unbound) = plan("ab", &layout, &[10, 11], 2);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].strokes,
            vec![stroke(8, false), stroke(10, false)]
        );
        assert_eq!(
            batches[0].bindings,
            vec![Binding {
                keycode: 10,
                unshifted: 0x62,
                shifted: 0
            }]
        );
    }

    #[test]
    fn plan_binds_each_distinct_character_once_and_reuses_repeats() {
        let layout = Layout::from_keysyms(&[], 2, 8);
        let (batches, unbound) = plan("中中", &layout, &[10, 11], 2);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].bindings,
            vec![Binding {
                keycode: 10,
                unshifted: 0x0100_4e2d,
                shifted: 0
            }]
        );
        assert_eq!(batches[0].strokes.len(), 2);
        assert_eq!(batches[0].strokes[0], batches[0].strokes[1]);
    }

    #[test]
    fn plan_puts_two_characters_on_one_spare_keycode_by_shift_level() {
        let layout = Layout::from_keysyms(&[], 2, 8);
        let (batches, unbound) = plan("中文", &layout, &[10, 11], 2);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].bindings,
            vec![Binding {
                keycode: 10,
                unshifted: 0x0100_4e2d,
                shifted: 0x0100_6587
            }]
        );
        assert_eq!(
            batches[0].strokes,
            vec![stroke(10, false), stroke(10, true)]
        );
    }

    #[test]
    fn plan_uses_one_level_per_keycode_when_the_mapping_has_no_shifted_level() {
        let layout = Layout::from_keysyms(&[], 1, 8);
        let (batches, unbound) = plan("中文", &layout, &[10, 11], 1);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].bindings,
            vec![
                Binding {
                    keycode: 10,
                    unshifted: 0x0100_4e2d,
                    shifted: 0
                },
                Binding {
                    keycode: 11,
                    unshifted: 0x0100_6587,
                    shifted: 0
                },
            ]
        );
        assert_eq!(
            batches[0].strokes,
            vec![stroke(10, false), stroke(11, false)]
        );
    }

    #[test]
    fn plan_rebinds_the_same_keycodes_once_the_batch_is_full() {
        let layout = Layout::from_keysyms(&[], 2, 8);
        // One spare keycode, two levels: the third distinct character needs a
        // second mapping change.
        let (batches, unbound) = plan("中文好", &layout, &[10], 2);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[0].bindings,
            vec![Binding {
                keycode: 10,
                unshifted: 0x0100_4e2d,
                shifted: 0x0100_6587
            }]
        );
        assert_eq!(
            batches[0].strokes,
            vec![stroke(10, false), stroke(10, true)]
        );
        assert_eq!(
            batches[1].bindings,
            vec![Binding {
                keycode: 10,
                unshifted: 0x0100_597d,
                shifted: 0
            }]
        );
        assert_eq!(batches[1].strokes, vec![stroke(10, false)]);
    }

    #[test]
    fn plan_types_a_sentence_of_unmapped_characters_in_one_keymap_change() {
        // The unused keycodes an Xorg session reports: fifteen of them, one run
        // of two and the rest singletons. Two levels each covers thirty
        // characters, so a dictated sentence needs one mapping change, not one
        // per character - the difference between typing in a burst and typing
        // a character at a time.
        let spares = [
            8u8, 97, 103, 120, 132, 149, 154, 168, 178, 183, 184, 219, 222, 230, 248,
        ];
        let layout = Layout::from_keysyms(&[], 4, 8);
        let text = "今天天气很好我们一起去公园散步看看花吧";
        let distinct = text.chars().collect::<std::collections::HashSet<_>>().len();
        assert_eq!(distinct, 17);

        let (batches, unbound) = plan(text, &layout, &spares, 2);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].bindings.len(), 9);
        assert_eq!(batches[0].strokes.len(), text.chars().count());
    }

    #[test]
    fn plan_binds_spare_keycodes_across_a_bound_one() {
        let layout = Layout::from_keysyms(&[], 2, 8);
        // keycodes 9 and 11 are spare, 10 is bound by the layout
        let keysyms = [0x61, 0, 0, 0, 0x62, 0, 0, 0];
        assert_eq!(spare_keycodes(&keysyms, 2, 8), vec![9, 11]);
        let (batches, unbound) = plan("中文好", &layout, &[9, 11], 2);
        assert_eq!(unbound, 0);
        assert_eq!(batches.len(), 1);
        // The batch binds keycode 11 as well, so the request has to cover the
        // bound keycode 10 in between.
        assert_eq!(
            batches[0].bindings,
            vec![
                Binding {
                    keycode: 9,
                    unshifted: 0x0100_4e2d,
                    shifted: 0x0100_6587
                },
                Binding {
                    keycode: 11,
                    unshifted: 0x0100_597d,
                    shifted: 0
                },
            ]
        );
        assert_eq!(
            batches[0].strokes,
            vec![stroke(9, false), stroke(9, true), stroke(11, false)]
        );
    }

    #[test]
    fn plan_with_no_spare_keycodes_binds_nothing() {
        let layout = Layout::from_keysyms(&[], 2, 8);
        let (batches, unbound) = plan("中", &layout, &[], 2);
        assert!(batches.is_empty());
        assert_eq!(unbound, 1);
    }

    #[test]
    fn plan_skips_characters_with_no_keysym() {
        let layout = Layout::from_keysyms(&[], 2, 8);
        let (batches, unbound) = plan("\u{7}", &layout, &[10], 2);
        assert!(batches.is_empty());
        assert_eq!(unbound, 0);
    }

    #[test]
    fn mixed_text_keeps_order() {
        let layout = Layout::from_keysyms(&[0x61, 0, 0, 0], 2, 8);
        let (batches, unbound) = plan("a中b", &layout, &[10, 11], 2);
        assert_eq!(unbound, 0);
        assert_eq!(
            batches[0].strokes,
            vec![stroke(8, false), stroke(10, false), stroke(10, true)]
        );
        assert_eq!(
            batches[0].bindings,
            vec![Binding {
                keycode: 10,
                unshifted: 0x0100_4e2d,
                shifted: 0x62
            }]
        );
    }

    #[test]
    fn span_keysyms_writes_the_bound_levels_over_the_original_ones() {
        // keycode 9's four levels; the binding owns levels 0 and 1, levels 2
        // and 3 keep what the server reported.
        let original = [0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68];
        let bindings = [Binding {
            keycode: 9,
            unshifted: 0x0100_4e2d,
            shifted: 0x0100_6587,
        }];
        assert_eq!(
            span_keysyms(&bindings, &original, 4, 8),
            vec![0x0100_4e2d, 0x0100_6587, 0x67, 0x68]
        );
    }

    #[test]
    fn span_keysyms_restates_the_keycodes_between_two_bindings() {
        // A request cannot skip keycode 9, which the layout binds, so its
        // keysyms are written back unchanged.
        let original = [
            0x61, 0x62, 0x63, 0x64, // keycode 8
            0x65, 0x66, 0x67, 0x68, // keycode 9
            0x69, 0x6a, 0x6b, 0x6c, // keycode 10
        ];
        let bindings = [
            Binding {
                keycode: 8,
                unshifted: 0x0100_4e2d,
                shifted: 0x0100_6587,
            },
            Binding {
                keycode: 10,
                unshifted: 0x0100_597d,
                shifted: 0,
            },
        ];
        assert_eq!(
            span_keysyms(&bindings, &original, 4, 8),
            vec![
                0x0100_4e2d,
                0x0100_6587,
                0x63,
                0x64, // keycode 8
                0x65,
                0x66,
                0x67,
                0x68, // keycode 9, untouched
                0x0100_597d,
                0,
                0x6b,
                0x6c, // keycode 10
            ]
        );
    }

    #[test]
    fn original_keysyms_returns_the_whole_span() {
        let original = [0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68];
        assert_eq!(
            original_keysyms(&original, 8, 9, 4, 8),
            vec![0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68]
        );
        assert_eq!(
            original_keysyms(&original, 9, 9, 4, 8),
            vec![0x65, 0x66, 0x67, 0x68]
        );
    }
}
