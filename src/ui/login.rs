//! Login screen: server URL, username, password, "Remember me".
//! On success emits [`LoggedIn`]; persistence goes to the Keychain only.

use super::text_input::TextInput;
use super::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding,
    Render, Window, actions, div, prelude::*, px, rgba,
};
use ztriton::jellyfin::Jellyfin;
use ztriton::keychain::{self, SavedLogin};

actions!(login, [Submit, FocusNext, FocusPrev, ToggleRemember]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("LoginView");
    cx.bind_keys([
        KeyBinding::new("enter", Submit, c),
        KeyBinding::new("tab", FocusNext, c),
        KeyBinding::new("shift-tab", FocusPrev, c),
    ]);
}

pub struct LoggedIn(pub Jellyfin);

pub struct LoginView {
    focus: FocusHandle,
    server: Entity<TextInput>,
    username: Entity<TextInput>,
    password: Entity<TextInput>,
    remember: bool,
    busy: bool,
    error: Option<String>,
}

impl EventEmitter<LoggedIn> for LoginView {}

impl LoginView {
    /// `prefill`: last server/username from the Keychain; `notice`: e.g.
    /// "Your saved session expired".
    pub fn new(prefill: Option<SavedLogin>, notice: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let server = cx.new(|cx| TextInput::new("http://jellyfin.local:8096", false, cx));
        let username = cx.new(|cx| TextInput::new("Username", false, cx));
        let password = cx.new(|cx| TextInput::new("Password", true, cx));
        if let Some(p) = &prefill {
            server.update(cx, |i, cx| i.set_text(p.server.clone(), cx));
            username.update(cx, |i, cx| i.set_text(p.username.clone(), cx));
        }
        let this = Self {
            focus: cx.focus_handle(),
            server,
            username,
            password,
            remember: true,
            busy: false,
            error: notice,
        };
        let first_empty = this.fields().into_iter().find(|f| f.read(cx).text().is_empty()).unwrap_or(this.password.clone());
        window.focus(&first_empty.focus_handle(cx));
        this
    }

    fn fields(&self) -> [Entity<TextInput>; 3] {
        [self.server.clone(), self.username.clone(), self.password.clone()]
    }

    fn cycle_focus(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields();
        let cur = fields.iter().position(|f| f.focus_handle(cx).is_focused(window)).unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(fields.len() as isize) as usize;
        window.focus(&fields[next].focus_handle(cx));
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(1, window, cx);
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_focus(-1, window, cx);
    }

    fn toggle_remember(&mut self, _: &ToggleRemember, _: &mut Window, cx: &mut Context<Self>) {
        self.remember = !self.remember;
        cx.notify();
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let server = self.server.read(cx).text().trim().to_string();
        let username = self.username.read(cx).text().trim().to_string();
        let password = self.password.read(cx).text();
        if server.is_empty() || username.is_empty() {
            self.error = Some("Enter a server address and username.".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let remember = self.remember;
        proof_log(format!("login: submitting user='{username}' server='{server}' remember={remember}"));
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let jf = Jellyfin::login(&server, &username, &password)?;
                    // Keychain I/O can block (e.g. an access prompt) — keep it off the UI thread.
                    let saved = SavedLogin {
                        server: jf.base.clone(),
                        username,
                        session: remember.then(|| jf.saved_session()),
                    };
                    if let Err(e) = keychain::save(&saved) {
                        log::warn!("{e:#}");
                        proof_log(format!("login: keychain save FAILED: {e:#}"));
                    } else {
                        proof_log(format!(
                            "login: keychain saved (service={}, token stored={})",
                            keychain::service(),
                            saved.session.is_some()
                        ));
                    }
                    anyhow::Ok(jf)
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match res {
                    Ok(jf) => {
                        proof_log(format!("login: success user_id={}", jf.user_id));
                        cx.emit(LoggedIn(jf));
                    }
                    Err(e) => {
                        proof_log(format!("login: failed: {e:#}"));
                        this.error = Some(format!("{e:#}"));
                        this.password.update(cx, |p, cx| p.set_text("", cx));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn label(text_: &'static str) -> impl IntoElement {
        div().text_sm().text_color(text_dim()).child(text_)
    }
}

impl Focusable for LoginView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LoginView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy;
        let remember = self.remember;
        div()
            .key_context("LoginView")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_action(cx.listener(Self::toggle_remember))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(bg())
            .child(
                div()
                    .id("login-card")
                    .w(px(400.))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_8()
                    .rounded_xl()
                    .bg(surface())
                    .shadow_lg()
                    .border_1()
                    .border_color(rgba(0xffffff12))
                    .child(div().text_3xl().font_weight(FontWeight::BOLD).text_color(text()).child("ztriton"))
                    .child(div().text_sm().text_color(text_dim()).pb_2().child("Sign in to your Jellyfin server"))
                    .child(div().flex().flex_col().gap_1().child(Self::label("Server")).child(self.server.clone()))
                    .child(div().flex().flex_col().gap_1().child(Self::label("Username")).child(self.username.clone()))
                    .child(div().flex().flex_col().gap_1().child(Self::label("Password")).child(self.password.clone()))
                    .child(
                        div()
                            .id("remember")
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, w, cx| this.toggle_remember(&ToggleRemember, w, cx)))
                            .child(
                                div()
                                    .size(px(18.))
                                    .rounded_sm()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_xs()
                                    .border_1()
                                    .border_color(if remember { accent() } else { text_dim() })
                                    .when(remember, |d| d.bg(accent()).text_color(bg()).child("✓")),
                            )
                            .child(div().text_sm().text_color(text()).child("Remember me (saved in Keychain)")),
                    )
                    .children(self.error.clone().map(|e| {
                        div()
                            .id("login-error")
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgba(0xff4d4f26))
                            .text_sm()
                            .text_color(gpui::rgb(0xff8a8a))
                            .child(e)
                    }))
                    .child(
                        div()
                            .id("sign-in")
                            .mt_2()
                            .py_3()
                            .rounded_lg()
                            .flex()
                            .justify_center()
                            .font_weight(FontWeight::SEMIBOLD)
                            .bg(accent())
                            .text_color(bg())
                            .when(busy, |d| d.opacity(0.6))
                            .when(!busy, |d| d.cursor_pointer().hover(|s| s.opacity(0.85)))
                            .on_click(cx.listener(|this, _, w, cx| this.submit(&Submit, w, cx)))
                            .child(if busy { "Signing in…" } else { "Sign In" }),
                    ),
            )
    }
}
