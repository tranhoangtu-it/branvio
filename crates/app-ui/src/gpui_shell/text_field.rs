//! Reusable focusable pure-GPUI text input supporting single-line search/branch boxes
//! and multi-line commit message/editor areas with UTF-8 cursor tracking and OS clipboard.

use crate::gpui_shell::to_rgba;
use crate::theme::BranvioTheme;
use gpui::{
    ClipboardItem, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, SharedString, Styled, Window, div, px,
};
use std::rc::Rc;

type TextCallback = Rc<dyn Fn(&str, &mut Window, &mut gpui::App)>;

/// Keyboard-focusable text field entity backed by GPUI events and OS clipboard integration.
pub struct BranvioTextField {
    focus_handle: FocusHandle,
    text: String,
    cursor_byte: usize,
    placeholder: SharedString,
    multiline: bool,
    on_change: Option<TextCallback>,
    on_submit: Option<TextCallback>,
}

impl BranvioTextField {
    /// Constructs a new [`BranvioTextField`].
    pub fn new(
        placeholder: impl Into<SharedString>,
        multiline: bool,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            text: String::new(),
            cursor_byte: 0,
            placeholder: placeholder.into(),
            multiline,
            on_change: None,
            on_submit: None,
        }
    }

    /// Registers a change listener fired after every edit.
    pub fn on_change(mut self, f: impl Fn(&str, &mut Window, &mut gpui::App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }

    /// Registers a submit listener (`Enter` on single-line or `Cmd/Ctrl+Enter` on multi-line).
    pub fn on_submit(mut self, f: impl Fn(&str, &mut Window, &mut gpui::App) + 'static) -> Self {
        self.on_submit = Some(Rc::new(f));
        self
    }

    /// Returns the current field text.
    #[inline]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Programmatically replaces text content and moves cursor to end.
    pub fn set_text(&mut self, value: String, cx: &mut Context<'_, Self>) {
        if self.text != value {
            self.text = value;
            self.cursor_byte = self.text.len();
            cx.notify();
        }
    }

    /// Moves window focus onto this field.
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    fn clamp_cursor(&mut self) {
        self.cursor_byte = self.cursor_byte.min(self.text.len());
        while self.cursor_byte > 0 && !self.text.is_char_boundary(self.cursor_byte) {
            self.cursor_byte -= 1;
        }
    }

    fn insert_str(&mut self, s: &str, window: &mut Window, cx: &mut Context<'_, Self>) {
        let filtered: String = if self.multiline {
            s.replace('\r', "")
        } else {
            s.replace(['\r', '\n'], " ")
        };
        if filtered.is_empty() {
            return;
        }
        self.clamp_cursor();
        self.text.insert_str(self.cursor_byte, &filtered);
        self.cursor_byte += filtered.len();
        self.notify_change(window, cx);
    }

    fn backspace(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.clamp_cursor();
        if self.cursor_byte == 0 {
            return;
        }
        let mut prev = self.cursor_byte - 1;
        while prev > 0 && !self.text.is_char_boundary(prev) {
            prev -= 1;
        }
        self.text.drain(prev..self.cursor_byte);
        self.cursor_byte = prev;
        self.notify_change(window, cx);
    }

    fn delete_forward(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.clamp_cursor();
        if self.cursor_byte >= self.text.len() {
            return;
        }
        let mut next = self.cursor_byte + 1;
        while next < self.text.len() && !self.text.is_char_boundary(next) {
            next += 1;
        }
        self.text.drain(self.cursor_byte..next);
        self.notify_change(window, cx);
    }

    fn move_left(&mut self, cx: &mut Context<'_, Self>) {
        self.clamp_cursor();
        if self.cursor_byte > 0 {
            let mut prev = self.cursor_byte - 1;
            while prev > 0 && !self.text.is_char_boundary(prev) {
                prev -= 1;
            }
            self.cursor_byte = prev;
            cx.notify();
        }
    }

    fn move_right(&mut self, cx: &mut Context<'_, Self>) {
        self.clamp_cursor();
        if self.cursor_byte < self.text.len() {
            let mut next = self.cursor_byte + 1;
            while next < self.text.len() && !self.text.is_char_boundary(next) {
                next += 1;
            }
            self.cursor_byte = next;
            cx.notify();
        }
    }

    fn notify_change(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(cb) = self.on_change.clone() {
            let text_clone = self.text.clone();
            cb(&text_clone, window, cx);
        }
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mods = &ev.keystroke.modifiers;
        let primary_mod = if cfg!(target_os = "macos") {
            mods.platform
        } else {
            mods.control
        };

        let key = ev.keystroke.key.as_str();
        if primary_mod {
            match key {
                "v" => {
                    if let Some(item) = cx.read_from_clipboard()
                        && let Some(clip_text) = item.text()
                    {
                        self.insert_str(&clip_text, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "c" => {
                    if !self.text.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(self.text.clone()));
                    }
                    cx.stop_propagation();
                    return;
                }
                "x" => {
                    if !self.text.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(self.text.clone()));
                        self.text.clear();
                        self.cursor_byte = 0;
                        self.notify_change(window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "a" => {
                    self.cursor_byte = self.text.len();
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "enter" if self.multiline => {
                    if let Some(cb) = self.on_submit.clone() {
                        let text_clone = self.text.clone();
                        cb(&text_clone, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => return,
            }
        }

        match key {
            "backspace" => {
                self.backspace(window, cx);
                cx.stop_propagation();
            }
            "delete" => {
                self.delete_forward(window, cx);
                cx.stop_propagation();
            }
            "left" => {
                self.move_left(cx);
                cx.stop_propagation();
            }
            "right" => {
                self.move_right(cx);
                cx.stop_propagation();
            }
            "home" => {
                self.cursor_byte = 0;
                cx.notify();
                cx.stop_propagation();
            }
            "end" => {
                self.cursor_byte = self.text.len();
                cx.notify();
                cx.stop_propagation();
            }
            "enter" => {
                if self.multiline {
                    self.insert_str("\n", window, cx);
                } else if let Some(cb) = self.on_submit.clone() {
                    let text_clone = self.text.clone();
                    cb(&text_clone, window, cx);
                }
                cx.stop_propagation();
            }
            _ => {
                if !mods.control
                    && !mods.alt
                    && !mods.platform
                    && !mods.function
                    && let Some(ref ch) = ev.keystroke.key_char
                    && !ch.is_empty()
                {
                    self.insert_str(ch, window, cx);
                    cx.stop_propagation();
                }
            }
        }
    }

    /// Renders this text field inside the supplied [`BranvioTheme`].
    pub fn render_themed(&self, theme: &BranvioTheme, window: &Window) -> impl IntoElement {
        let is_focused = self.focus_handle.is_focused(window);
        let border = if is_focused {
            theme.border_focus
        } else {
            theme.border_subtle
        };

        let mut body = div()
            .flex()
            .items_center()
            .w_full()
            .min_h(px(if self.multiline { 68.0 } else { 28.0 }))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .bg(to_rgba(theme.bg_base))
            .border_1()
            .border_color(to_rgba(border))
            .text_size(px(12.0));

        if self.text.is_empty() {
            body = body
                .text_color(to_rgba(theme.text_muted))
                .child(self.placeholder.clone());
            if is_focused {
                body = body.child(
                    div()
                        .w(px(1.5))
                        .h(px(14.0))
                        .ml(px(1.0))
                        .bg(to_rgba(theme.accent_primary)),
                );
            }
        } else {
            let safe_idx = self.cursor_byte.min(self.text.len());
            let (before, after) = if self.text.is_char_boundary(safe_idx) {
                (&self.text[..safe_idx], &self.text[safe_idx..])
            } else {
                (self.text.as_str(), "")
            };
            body = body
                .text_color(to_rgba(theme.text_primary))
                .child(before.to_string());
            if is_focused {
                body = body.child(
                    div()
                        .w(px(1.5))
                        .h(px(14.0))
                        .bg(to_rgba(theme.accent_primary)),
                );
            }
            if !after.is_empty() {
                body = body.child(after.to_string());
            }
        }

        body
    }
}

impl Focusable for BranvioTextField {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranvioTextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = BranvioTheme::dark();
        div()
            .track_focus(&self.focus_handle)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, window, _cx| {
                    window.focus(&this.focus_handle);
                }),
            )
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(self.render_themed(&theme, window))
    }
}
