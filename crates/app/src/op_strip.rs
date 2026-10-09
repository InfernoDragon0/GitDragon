//! Operation strip along the bottom of the window: the git command running now (or the last
//! one, with its time and outcome), and Cancel for a running user operation.

use std::time::Duration;

use git_core::runner::Outcome;
use git_core::{CommandRecord, GitRunner, Mode};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, h_flex};
use gpui_kit::{
    ClickEvent, Context, EventEmitter, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, div, px,
};
use ui::Palette;

pub enum OpStripEvent {
    Cancel,
}

pub struct OpStrip {
    /// Running user (write) operation, if any.
    running: Option<CommandRecord>,
    last: Option<CommandRecord>,
    /// Right-hand text: branch and ahead / behind of the active tab.
    branch: Option<SharedString>,
    _listen: Option<Task<()>>,
}

impl EventEmitter<OpStripEvent> for OpStrip {}

impl OpStrip {
    pub fn new() -> Self {
        Self {
            running: None,
            last: None,
            branch: None,
            _listen: None,
        }
    }

    /// Follows `runner`'s command log (the strip is its only listener).
    pub fn listen(&mut self, runner: &GitRunner, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::unbounded::<CommandRecord>();
        runner.log().set_listener(move |record| {
            _ = tx.try_send(record.clone());
        });
        self._listen = Some(cx.spawn(async move |this, cx| {
            while let Ok(mut record) = rx.recv().await {
                // Keep only the newest of a burst (background refreshes come in groups).
                let mut batch = vec![];
                while let Ok(more) = rx.try_recv() {
                    batch.push(std::mem::replace(&mut record, more));
                }
                batch.push(record);
                if this
                    .update(cx, |strip, cx| {
                        for record in batch {
                            strip.on_record(record);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn on_record(&mut self, record: CommandRecord) {
        let write = record.mode == Mode::Write;
        if record.outcome == Outcome::Running {
            if write {
                self.running = Some(record);
            }
            return;
        }
        if self.running.as_ref().is_some_and(|r| r.id == record.id) {
            self.running = None;
        }
        // Background reads only replace the last line when no write result is showing.
        let keep_write = self
            .last
            .as_ref()
            .is_some_and(|l| l.mode == Mode::Write && !write);
        if !keep_write || !matches!(record.outcome, Outcome::Ok) {
            self.last = Some(record);
        }
    }

    pub fn set_branch(&mut self, branch: Option<String>, cx: &mut Context<Self>) {
        let branch = branch.map(SharedString::from);
        if self.branch != branch {
            self.branch = branch;
            cx.notify();
        }
    }
}

fn millis(d: Option<Duration>) -> String {
    d.map(|d| format!(" ({} ms)", d.as_millis()))
        .unwrap_or_default()
}

impl Render for OpStrip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(cx);
        let (dot, text) = match (&self.running, &self.last) {
            (Some(run), _) => (
                palette.now,
                match &run.progress {
                    Some(progress) => format!("Running: {} — {progress}", run.command),
                    None => format!("Running: {}", run.command),
                },
            ),
            (None, Some(last)) => match last.outcome {
                Outcome::Ok | Outcome::Running => (
                    palette.ok,
                    format!("Idle · last: {}{}", last.command, millis(last.duration)),
                ),
                Outcome::Cancelled => (palette.warn, format!("Cancelled: {}", last.command)),
                Outcome::TimedOut => (palette.error, format!("Timed out: {}", last.command)),
                Outcome::Failed { code } => (
                    palette.error,
                    format!(
                        "Failed (exit {}): {}",
                        code.map_or("?".into(), |c| c.to_string()),
                        last.command
                    ),
                ),
            },
            (None, None) => (palette.ok, "Idle".to_string()),
        };
        h_flex()
            .h(px(24.))
            .w_full()
            .px_2()
            .gap_2()
            .border_t_1()
            .border_color(palette.border)
            .bg(palette.panel)
            .text_xs()
            .text_color(palette.muted)
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(text)),
            )
            .children(self.running.is_some().then(|| {
                Button::new("op-cancel")
                    .ghost()
                    .xsmall()
                    .label("Cancel")
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(OpStripEvent::Cancel)))
            }))
            .children(
                self.branch
                    .clone()
                    .map(|b| div().text_color(palette.fg).child(b)),
            )
    }
}
