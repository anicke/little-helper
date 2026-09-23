use crossterm::event::KeyCode;

// --- Tag / Rename ----------------------------------------------------------------------
//
// The first screens in this repo that edit rather than watch (docs/tagging.md §6). Every
// earlier screen submits jobs to a `Queue` and folds events into a table; nothing above
// takes keyboard input beyond quitting. Two things follow, both recorded in
// `docs/tui.md`'s editor-screen section:
//
// * **`q` cannot mean quit while a field has focus** — it is a letter someone is typing.
//   `Esc` leaves the focused field (or, from no field, leaves the screen); `Ctrl-C`
//   always aborts, everywhere; `q` quits only when nothing is focused.
// * **Bracketed paste is enabled for the lifetime of these two screens only** —
//   `ratatui::init()` does not turn it on, so a pasted setlist would otherwise arrive as
//   individual `Char` events racing the 80ms poll loop.

/// A single-line text input: the value plus a cursor kept as a *character* index (not a
/// byte one), so it walks a non-ASCII title correctly. No new dependency — every field on
/// both screens below is a handful of `char`/`Backspace`/arrow-key cases away from a
/// `String`.
#[derive(Clone, Default)]
pub(crate) struct Field {
    pub(crate) value: String,
    pub(crate) cursor: usize,
}

impl Field {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Field { value, cursor }
    }

    /// An empty field means *leave this alone* — the same thing a CLI flag left unset
    /// means for `Tags`/`TagArgs` (docs/tagging.md §4) — not *set it to the empty
    /// string*. There is deliberately no way to force-clear a field from this screen;
    /// `lh tag --artist ''` still can, from the CLI.
    pub(crate) fn edit_value(&self) -> Option<String> {
        (!self.value.is_empty()).then(|| self.value.clone())
    }

    pub(crate) fn byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// A thin cursor glyph spliced into the text at render time — not a real terminal
    /// cursor, which would need this widget to know its own screen coordinates inside
    /// whatever layout is drawing it.
    pub(crate) fn display(&self, focused: bool) -> String {
        if !focused {
            return self.value.clone();
        }
        let idx = self.byte_index();
        let mut s = String::with_capacity(self.value.len() + 3);
        s.push_str(&self.value[..idx]);
        s.push('▏');
        s.push_str(&self.value[idx..]);
        s
    }

    /// Handles one key; `false` means this field had nothing to do with it.
    pub(crate) fn on_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char(c) => {
                let idx = self.byte_index();
                self.value.insert(idx, c);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let idx = self.byte_index();
                    self.value.remove(idx);
                }
                true
            }
            KeyCode::Delete => {
                let idx = self.byte_index();
                if idx < self.value.len() {
                    self.value.remove(idx);
                }
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                true
            }
            _ => false,
        }
    }
}

/// Splits a pasted block on newlines into exactly `len` titles, padding with blanks or
/// truncating extras. Unlike `cmd_tag`'s one-shot `--titles FILE` read, which errors out
/// on a count mismatch (docs/tagging.md §5), this is a live editable field — a paste that
/// is briefly the wrong length is finished by editing afterwards, not by failing the
/// screen.
///
/// Many terminals (xterm, VTE, …) send pasted newlines as bare `\r`, which `str::lines`
/// does not split on, so `\r\n` and `\r` are normalised to `\n` first.
pub(crate) fn paste_titles(text: &str, len: usize) -> Vec<Field> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut fields: Vec<Field> = text.lines().map(Field::new).collect();
    fields.resize_with(len, Field::default);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(fields: &[Field]) -> Vec<&str> {
        fields.iter().map(|f| f.value.as_str()).collect()
    }

    #[test]
    fn paste_titles_splits_every_newline_style() {
        for text in [
            "One\nTwo\nThree\n",
            "One\r\nTwo\r\nThree\r\n",
            "One\rTwo\rThree\r",
        ] {
            assert_eq!(values(&paste_titles(text, 3)), ["One", "Two", "Three"]);
        }
    }

    #[test]
    fn paste_titles_pads_and_truncates_to_the_track_count() {
        assert_eq!(values(&paste_titles("One\rTwo", 3)), ["One", "Two", ""]);
        assert_eq!(values(&paste_titles("One\rTwo\rThree", 2)), ["One", "Two"]);
    }
}
