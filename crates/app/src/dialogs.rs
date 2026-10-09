//! Small shared dialogs: a confirmation (danger-styled when it destroys something) and a text
//! prompt with one or more fields and one or more submit buttons.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, ElementId, Entity, Focusable as _, ParentElement as _,
    SharedString, Styled as _, Subscription, Window, px,
};

/// Asks before a destructive or surprising action; `on_ok` runs only on confirmation.
pub fn confirm(
    window: &mut Window,
    cx: &mut App,
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    ok: impl Into<SharedString>,
    danger: bool,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    let (title, body, ok) = (title.into(), body.into(), ok.into());
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        alert
            .confirm()
            .title(title.clone())
            .description(body.clone())
            .ok_text(ok.clone())
            .ok_variant(if danger {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Primary
            })
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
}

/// Submits a [`prompt`] with the index of the pressed button.
type Submit = dyn Fn(usize, &mut Window, &mut App);

/// One input of a [`prompt`].
pub struct Field {
    pub placeholder: &'static str,
    pub value: String,
}

impl Field {
    pub fn new(placeholder: &'static str) -> Self {
        Self {
            placeholder,
            value: String::new(),
        }
    }

    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }
}

/// Text prompt. `buttons` are submit buttons, last = primary; `on_submit` gets the trimmed field
/// values and the index of the button (Enter = last, Ctrl+Enter = first). Submitting with an
/// empty first field does nothing. The returned subscription keeps Enter working: hold it until
/// the dialog closes.
pub fn prompt<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    title: impl Into<SharedString>,
    fields: Vec<Field>,
    buttons: &[&'static str],
    on_submit: impl Fn(Vec<String>, usize, &mut Window, &mut App) + 'static,
) -> Subscription {
    let inputs: Vec<Entity<InputState>> = fields
        .into_iter()
        .map(|f| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(f.placeholder)
                    .default_value(f.value)
            })
        })
        .collect();
    let buttons: Vec<&'static str> = buttons.to_vec();
    let submit: Rc<Submit> = {
        let inputs = inputs.clone();
        Rc::new(move |button, window, cx| {
            let values: Vec<String> = inputs
                .iter()
                .map(|i| i.read(cx).value().trim().to_string())
                .collect();
            if values.first().is_none_or(String::is_empty) {
                return;
            }
            on_submit(values, button, window, cx);
            window.close_dialog(cx);
        })
    };
    let last = buttons.len().saturating_sub(1);
    let sub = cx.subscribe_in(&inputs[0], window, {
        let submit = submit.clone();
        move |_, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary, .. } = event {
                submit(if *secondary { 0 } else { last }, window, cx);
            }
        }
    });
    let title: SharedString = title.into();
    let focus = inputs[0].focus_handle(cx);
    window.open_dialog(cx, move |dialog, _, _| {
        let fields = v_flex().gap_2().children(inputs.iter().map(Input::new));
        let mut row = h_flex().justify_end().gap_2().child(
            Button::new("prompt-cancel")
                .ghost()
                .label("Cancel")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        );
        for (i, label) in buttons.iter().enumerate() {
            let submit = submit.clone();
            let button = Button::new(ElementId::NamedInteger("prompt-ok".into(), i as u64))
                .label(*label)
                .on_click(move |_, window, cx| submit(i, window, cx));
            row = row.child(if i == last {
                button.primary()
            } else {
                button.outline()
            });
        }
        dialog
            .title(title.clone())
            .w(px(440.))
            .child(fields)
            .footer(row)
    });
    window.focus(&focus, cx);
    sub
}
