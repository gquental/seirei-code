use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Context {
    Global,
    Picker,
    Browse,
    Compose,
    Confirm,
    Answer,
    AnswerText,
    AddMachine,
    Login,
    ConfirmRemove,
    Help,
    WrongBuild,
}

impl Context {
    pub fn takes_text(self) -> bool {
        matches!(self, Context::Compose | Context::AnswerText | Context::AddMachine | Context::Login)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    Back,
    Up,
    Down,
    Left,
    Right,
    Open,
    NextPane,
    Compose,
    Submit,
    Newline,
    CreateCoordinator,
    Confirm,
    AnswerQuestion,
    Toggle,
    AddMachine,
    LogIn,
    RemoveMachine,
    NextField,
    ToggleHelp,
    TakeOver,
}

pub struct Binding {
    pub context: Context,
    pub code: KeyCode,
    pub mods: KeyModifiers,
    pub action: Action,
    pub help: &'static str,
}

impl Binding {
    pub fn label(&self) -> String {
        let key = match self.code {
            Char(' ') => "space".into(),
            Char(c) => c.to_string(),
            Enter => "enter".into(),
            Esc => "esc".into(),
            Tab => "tab".into(),
            Up => "↑".into(),
            Down => "↓".into(),
            Left => "←".into(),
            Right => "→".into(),
            other => format!("{other:?}").to_lowercase(),
        };
        match self.mods {
            KeyModifiers::CONTROL => format!("ctrl-{key}"),
            KeyModifiers::ALT => format!("alt-{key}"),
            _ => key,
        }
    }
}

const fn key(context: Context, code: KeyCode, action: Action, help: &'static str) -> Binding {
    Binding { context, code, mods: KeyModifiers::NONE, action, help }
}

const fn chord(context: Context, mods: KeyModifiers, code: KeyCode, action: Action, help: &'static str) -> Binding {
    Binding { context, code, mods, action, help }
}

use Action as A;
use Context as C;
use KeyCode::{Char, Down, Enter, Esc, Left, Right, Tab, Up};

pub const BINDINGS: &[Binding] = &[
    chord(C::Global, KeyModifiers::CONTROL, Char('c'), A::Quit, "quit"),
    key(C::Picker, Char('j'), A::Down, "down"),
    key(C::Picker, Down, A::Down, "down"),
    key(C::Picker, Char('k'), A::Up, "up"),
    key(C::Picker, Up, A::Up, "up"),
    key(C::Picker, Enter, A::Open, "open project"),
    key(C::Picker, Char('m'), A::AddMachine, "add a machine"),
    key(C::Picker, Char('l'), A::LogIn, "log in"),
    key(C::Picker, Char('x'), A::RemoveMachine, "remove machine"),
    key(C::Picker, Char('?'), A::ToggleHelp, "help"),
    key(C::Picker, Char('q'), A::Quit, "quit"),
    key(C::Browse, Tab, A::NextPane, "switch pane"),
    key(C::Browse, Char('j'), A::Down, "down"),
    key(C::Browse, Down, A::Down, "down"),
    key(C::Browse, Char('k'), A::Up, "up"),
    key(C::Browse, Up, A::Up, "up"),
    key(C::Browse, Enter, A::Open, "open job timeline"),
    key(C::Browse, Char('i'), A::Compose, "message the coordinator"),
    key(C::Browse, Char('c'), A::CreateCoordinator, "create coordinator"),
    key(C::Browse, Char('a'), A::AnswerQuestion, "answer a question"),
    key(C::Browse, Char('?'), A::ToggleHelp, "help"),
    key(C::Browse, Esc, A::Back, "back"),
    key(C::Compose, Enter, A::Submit, "send"),
    chord(C::Compose, KeyModifiers::ALT, Enter, A::Newline, "new line"),
    key(C::Compose, Esc, A::Back, "keep draft and close"),
    key(C::Confirm, Char('y'), A::Confirm, "create"),
    key(C::Confirm, Char('n'), A::Back, "cancel"),
    key(C::Confirm, Esc, A::Back, "cancel"),
    key(C::Answer, Char('j'), A::Down, "next option"),
    key(C::Answer, Down, A::Down, "next option"),
    key(C::Answer, Char('k'), A::Up, "previous option"),
    key(C::Answer, Up, A::Up, "previous option"),
    key(C::Answer, Char(' '), A::Toggle, "toggle option"),
    key(C::Answer, Enter, A::Submit, "answer"),
    key(C::Answer, Esc, A::Back, "later"),
    key(C::AnswerText, Up, A::Up, "previous option"),
    key(C::AnswerText, Down, A::Down, "next option"),
    key(C::AnswerText, Enter, A::Submit, "answer"),
    key(C::AnswerText, Esc, A::Back, "later"),
    key(C::AddMachine, Tab, A::NextField, "next field"),
    key(C::AddMachine, Enter, A::Submit, "add"),
    key(C::AddMachine, Esc, A::Back, "cancel"),
    key(C::Login, Left, A::Left, "less access"),
    key(C::Login, Right, A::Right, "more access"),
    key(C::Login, Enter, A::Submit, "log in"),
    key(C::Login, Esc, A::Back, "cancel"),
    key(C::ConfirmRemove, Char('y'), A::Confirm, "remove"),
    key(C::ConfirmRemove, Char('n'), A::Back, "cancel"),
    key(C::ConfirmRemove, Esc, A::Back, "cancel"),
    key(C::Help, Char('?'), A::ToggleHelp, "close help"),
    key(C::Help, Esc, A::ToggleHelp, "close help"),
    key(C::WrongBuild, Char('r'), A::TakeOver, "restart the daemon"),
    key(C::WrongBuild, Char('q'), A::Quit, "quit"),
];

pub fn lookup(context: Context, event: KeyEvent) -> Option<Action> {
    [context, Context::Global].into_iter().find_map(|ctx| {
        (BINDINGS.iter())
            .find(|b| b.context == ctx && b.code == event.code && b.mods == event.modifiers)
            .map(|b| b.action)
    })
}

pub fn hints(context: Context) -> Vec<(String, Action, &'static str)> {
    let mut hints: Vec<(String, Action, &'static str)> = Vec::new();
    for b in BINDINGS.iter().filter(|b| b.context == context) {
        match hints.iter_mut().find(|(_, action, _)| *action == b.action) {
            Some((keys, _, _)) => *keys = format!("{keys}/{}", b.label()),
            None => hints.push((b.label(), b.action, b.help)),
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_is_bound_twice_in_one_context() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                let clash = a.code == b.code && a.mods == b.mods;
                let shadows = a.context == b.context || a.context == Context::Global || b.context == Context::Global;
                assert!(!(clash && shadows), "{:?} {:?} bound twice", a.context, a.code);
            }
        }
    }

    #[test]
    fn text_contexts_and_global_never_steal_typed_text() {
        for context in [Context::Compose, Context::AnswerText, Context::AddMachine, Context::Login] {
            assert!(context.takes_text(), "{context:?} takes text");
        }
        let typed = |b: &&Binding| {
            matches!(b.code, Char(_))
                && (b.mods == KeyModifiers::NONE || b.mods == KeyModifiers::SHIFT)
                && (b.context.takes_text() || b.context == Context::Global)
        };
        assert!(BINDINGS.iter().find(typed).is_none());
    }

    #[test]
    fn hints_merge_keys_bound_to_one_action() {
        let picker = hints(Context::Picker);
        assert_eq!(picker[0], ("j/↓".to_string(), Action::Down, "down"));
        assert_eq!(picker.len(), 8);
    }
}
