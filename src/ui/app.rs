//! Root view: navigation stack (Home -> Library -> Detail -> Player).

use super::login::{LoggedIn, LoginView};
use super::player_view::PlayerView;
use super::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, ObjectFit,
    Render, SharedString, Window, actions, div, img, linear_color_stop, linear_gradient,
    prelude::*, px, rgba,
};
use std::collections::HashMap;
use std::time::Duration;
use ztriton::jellyfin::{Item, Jellyfin};
use ztriton::keychain::{self, SavedLogin};

actions!(ztriton, [Back, Quit, ToggleFullscreen, SignOut]);

#[derive(Clone)]
enum Screen {
    Home,
    Library { parent: Item, items: Option<Arc<Vec<Item>>> },
    Detail(Item),
    Player(Entity<PlayerView>),
}

struct Shelf {
    title: String,
    items: Vec<Item>,
    wide: bool,
}

enum ImgState {
    Loading,
    Ready(Arc<gpui::RenderImage>),
    Failed,
}

pub struct ZtritonApp {
    jf: Option<Jellyfin>,
    /// Shown while signed out; `None` + `jf: None` means "restoring session".
    login: Option<Entity<LoginView>>,
    /// Bumped on every sign-in/out so stale background loads are dropped.
    generation: u64,
    error: Option<String>,
    stack: Vec<Screen>,
    shelves: Vec<Shelf>,
    views: Vec<Item>,
    images: HashMap<String, ImgState>,
    focus: FocusHandle,
    autoplay: Option<String>,
    title: String,
    /// Mirrors `window.is_fullscreen()` for the header button label.
    fullscreen: bool,
}

const POSTER_W: f32 = 150.;
const POSTER_H: f32 = 225.;
const WIDE_W: f32 = 280.;
const WIDE_H: f32 = 158.;

impl ZtritonApp {
    pub fn new(autoplay: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        let this = Self {
            jf: None,
            login: None,
            generation: 0,
            error: None,
            stack: vec![Screen::Home],
            shelves: vec![],
            views: vec![],
            images: HashMap::new(),
            focus,
            autoplay,
            title: String::new(),
            fullscreen: window.is_fullscreen(),
        };
        // Restore a remembered session from the Keychain, else show the login form.
        cx.spawn_in(window, async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let saved = keychain::load()?;
                    let restored = match saved.as_ref().and_then(|s| s.session.clone().map(|t| (s, t))) {
                        Some((s, t)) => Some(Jellyfin::resume_session(&s.server, &t.user_id, &t.token).map_err(|e| {
                            // Only a rejected token is forgotten; a server that is merely down keeps it.
                            let revoked = format!("{e:#}").contains("status code 401");
                            if revoked && let Err(e) = keychain::forget_session() {
                                log::warn!("could not forget revoked session: {e:#}");
                            }
                            (e, revoked)
                        })),
                        None => None,
                    };
                    anyhow::Ok((saved, restored))
                })
                .await;
            this.update_in(cx, |this, window, cx| match res {
                Ok((_, Some(Ok(jf)))) => {
                    proof_log(format!("auth: restored session from Keychain (service={})", keychain::service()));
                    this.signed_in(jf, window, cx)
                }
                Ok((saved, restored)) => {
                    let notice = match restored {
                        Some(Err((_, true))) => Some("Your saved session has expired. Please sign in again.".into()),
                        Some(Err((e, false))) => Some(format!("Couldn't restore saved session: {e:#}")),
                        _ => None,
                    };
                    proof_log(format!("auth: no usable saved session (prefill={}, notice={notice:?})", saved.is_some()));
                    this.show_login(saved, notice, window, cx)
                }
                Err(e) => this.show_login(None, Some(format!("{e:#}")), window, cx),
            })
            .log_err();
        })
        .detach();
        this
    }

    fn show_login(&mut self, prefill: Option<SavedLogin>, notice: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| LoginView::new(prefill, notice, window, cx));
        cx.subscribe_in(&view, window, |this, _, ev: &LoggedIn, window, cx| this.signed_in(ev.0.clone(), window, cx))
            .detach();
        self.login = Some(view);
        cx.notify();
    }

    fn signed_in(&mut self, jf: Jellyfin, window: &mut Window, cx: &mut Context<Self>) {
        self.login = None;
        self.generation += 1;
        self.error = None;
        self.stack = vec![Screen::Home];
        self.shelves.clear();
        self.views.clear();
        self.images.clear();
        self.jf = Some(jf.clone());
        window.focus(&self.focus);
        self.load_home(jf, cx);
        cx.notify();
    }

    fn sign_out(&mut self, _: &SignOut, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.stack.as_slice(), [Screen::Home]) {
            return; // only offered on Home (no player alive)
        }
        let Some(jf) = self.jf.take() else { return };
        self.generation += 1;
        proof_log(format!("auth: signing out user_id={}", jf.user_id));
        // Revoke the token server-side and drop it from the Keychain (keep server/username for prefill).
        let task = cx.background_executor().spawn(async move {
            let kc = keychain::forget_session();
            let srv = jf.logout();
            proof_log(format!("auth: keychain token removed: {:?}; server logout: {:?}", kc.is_ok(), srv.as_ref().map_err(|e| e.to_string())));
            keychain::load().ok().flatten()
        });
        cx.spawn_in(window, async move |this, cx| {
            let prefill = task.await;
            this.update_in(cx, |this, window, cx| this.show_login(prefill, None, window, cx)).log_err();
        })
        .detach();
        self.shelves.clear();
        self.views.clear();
        self.images.clear();
        cx.notify();
    }

    fn load_home(&mut self, jf: Jellyfin, cx: &mut Context<Self>) {
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let views: Vec<Item> = jf
                        .views()?
                        .into_iter()
                        .filter(|v| {
                            matches!(v.collection_type.as_deref(), Some("movies" | "tvshows" | "homevideos") | None)
                        })
                        .collect();
                    let mut shelves = vec![];
                    let resume = jf.resume(20).unwrap_or_default();
                    if !resume.is_empty() {
                        shelves.push(Shelf { title: "Continue Watching".into(), items: resume, wide: true });
                    }
                    for v in &views {
                        let items = jf.latest(&v.id, 20).unwrap_or_default();
                        if !items.is_empty() {
                            shelves.push(Shelf { title: format!("Recently Added in {}", v.name), items, wide: false });
                        }
                    }
                    anyhow::Ok((jf, views, shelves))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return; // signed out / in again meanwhile
                }
                match res {
                    Ok((jf, views, shelves)) => {
                        proof_log(format!(
                            "home: logged in to {} as user {}; {} libraries, shelves: {}",
                            jf.base,
                            jf.user_id,
                            views.len(),
                            shelves.iter().map(|s| format!("{} ({})", s.title, s.items.len())).collect::<Vec<_>>().join(", ")
                        ));
                        this.views = views;
                        this.shelves = shelves;
                        this.maybe_autoplay(cx);
                    }
                    Err(e) => this.error = Some(format!("{e:#}")),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn maybe_autoplay(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self.autoplay.take() else { return };
        let Some(jf) = self.jf.clone() else { return };
        cx.spawn(async move |this, cx| {
            // Let the home screen render (and be screenshotted) first.
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let search = (query != "first").then_some(query);
            let item = cx
                .background_executor()
                .spawn(async move { jf.first_video(search.as_deref()) })
                .await;
            let Ok(item) = item else {
                proof_log(format!("autoplay: no item found: {:?}", item.err()));
                return;
            };
            proof_log(format!("autoplay: opening detail for '{}' ({})", item.name, item.id));
            this.update(cx, |this, cx| {
                this.stack.push(Screen::Detail(item.clone()));
                cx.notify();
            })
            .log_err();
            cx.background_executor().timer(Duration::from_secs(4)).await;
            cx.update(|cx| {
                if let Some(window) = cx.windows().first().copied() {
                    window
                        .update(cx, |_, window, cx| {
                            this.update(cx, |this, cx| this.play(item, window, cx)).log_err();
                        })
                        .log_err();
                }
            })
            .log_err();
        })
        .detach();
    }

    fn image(&mut self, url: Option<String>, max_w: u32, cx: &mut Context<Self>) -> Option<Arc<gpui::RenderImage>> {
        let url = url?;
        match self.images.get(&url) {
            Some(ImgState::Ready(img)) => return Some(img.clone()),
            Some(_) => return None,
            None => {}
        }
        drop(self.images.insert(url.clone(), ImgState::Loading));
        let jf = self.jf.clone()?;
        cx.spawn(async move |this, cx| {
            let u = url.clone();
            let res = cx
                .background_executor()
                .spawn(async move { decode_render_image(&jf.fetch_bytes(&u)?, max_w) })
                .await;
            this.update(cx, |this, cx| {
                let state = match res {
                    Ok(i) => ImgState::Ready(i),
                    Err(_) => ImgState::Failed,
                };
                drop(this.images.insert(url, state));
                cx.notify();
            })
            .log_err();
        })
        .detach();
        None
    }

    fn open(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        if item.is_playable() {
            self.stack.push(Screen::Detail(item));
        } else if item.is_folder
            && let Some(jf) = self.jf.clone()
        {
            let id = item.id.clone();
            self.stack.push(Screen::Library { parent: item, items: None });
            let depth = self.stack.len();
            cx.spawn(async move |this, cx| {
                let id2 = id.clone();
                let items = cx
                    .background_executor()
                    .spawn(async move { jf.children(&id2) })
                    .await
                    .unwrap_or_default();
                this.update(cx, |this, cx| {
                    // Only fill the slot if it is still the same library (user may have navigated).
                    if let Some(Screen::Library { parent, items: slot }) = this.stack.get_mut(depth - 1)
                        && parent.id == id
                    {
                        *slot = Some(Arc::new(items));
                    }
                    cx.notify();
                })
                .log_err();
            })
            .detach();
        }
        let _ = window;
        cx.notify();
    }

    fn play(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        let Some(jf) = self.jf.clone() else { return };
        let view = cx.new(|cx| PlayerView::new(jf, item, window, cx));
        cx.subscribe_in(&view, window, |this, _, _: &super::player_view::Closed, window, cx| {
            if matches!(this.stack.last(), Some(Screen::Player(_))) {
                this.pop(cx);
            }
            window.focus(&this.focus);
        })
        .detach();
        let fh = view.read(cx).focus_handle(cx);
        window.focus(&fh);
        self.stack.push(Screen::Player(view));
        cx.notify();
    }

    fn pop(&mut self, cx: &mut Context<Self>) {
        if self.stack.len() > 1 {
            drop(self.stack.pop());
        }
        cx.notify();
    }

    fn back(&mut self, _: &Back, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Screen::Player(_)) = self.stack.last() {
            // Player handles its own Esc (reports stop, then emits Closed).
            return;
        }
        self.pop(cx);
        window.focus(&self.focus);
    }

    // ------------------------------------------------------------------ views

    fn poster_card(&mut self, item: &Item, wide: bool, cx: &mut Context<Self>) -> AnyElement {
        let (w, h) = if wide { (WIDE_W, WIDE_H) } else { (POSTER_W, POSTER_H) };
        let kind = if wide { "Thumb" } else { "Primary" };
        let image_width = retina_pixels(w);
        let url = self
            .jf
            .as_ref()
            .and_then(|jf| {
                jf.image_url(item, kind, image_width)
                    .or_else(|| jf.image_url(item, "Primary", image_width))
                    .or_else(|| jf.image_url(item, "Backdrop", image_width))
            });
        let image = self.image(url, image_width, cx);
        let progress = item.user_data.as_ref().and_then(|u| u.played_percentage).unwrap_or(0.0);
        let played = item.user_data.as_ref().map(|u| u.played).unwrap_or(false);
        let it = item.clone();

        div()
            .id(SharedString::from(format!("card-{}-{}", item.id, wide)))
            .flex()
            .flex_col()
            .gap_2()
            .w(px(w))
            .flex_none()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.open(it.clone(), window, cx)))
            .child(
                div()
                    .relative()
                    .w(px(w))
                    .h(px(h))
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(surface_hi())
                    .shadow_lg()
                    .hover(|s| s.border_2().border_color(accent()))
                    .child(match image {
                        Some(i) => img(i).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                        None => div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .p_2()
                            .text_color(text_dim())
                            .text_sm()
                            .child(item.name.clone())
                            .into_any_element(),
                    })
                    .when(progress > 0.0 && progress < 100.0, |d| {
                        d.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .w_full()
                                .h(px(4.))
                                .bg(rgba(0x00000099))
                                .child(div().h_full().w(px(w * progress as f32 / 100.)).bg(accent())),
                        )
                    })
                    .when(played, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top_1()
                                .right_1()
                                .px_1()
                                .rounded_sm()
                                .bg(accent())
                                .text_xs()
                                .text_color(bg())
                                .child("✓"),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().text_color(text()).truncate().child(item.name.clone()))
                    .child(div().text_xs().text_color(text_dim()).truncate().child(item.subtitle())),
            )
            .into_any_element()
    }

    fn header(&self, title: String, can_back: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_4()
            .px_8()
            .pt_8()
            .pb_4()
            .when(can_back, |d| {
                d.child(
                    div()
                        .id("back")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(surface_hi())
                        .cursor_pointer()
                        .hover(|s| s.bg(accent()).text_color(bg()))
                        .child("‹ Back")
                        .on_click(cx.listener(|this, _, window, cx| this.back(&Back, window, cx))),
                )
            })
            .child(div().text_3xl().font_weight(FontWeight::BOLD).text_color(text()).child(title))
            .child(div().flex_1())
            .when(!can_back && self.jf.is_some(), |d| {
                d.child(
                    div()
                        .id("sign-out")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(surface_hi())
                        .text_sm()
                        .text_color(text_dim())
                        .cursor_pointer()
                        .hover(|s| s.bg(accent()).text_color(bg()))
                        .child("Sign Out")
                        .on_click(cx.listener(|this, _, window, cx| this.sign_out(&SignOut, window, cx))),
                )
            })
            .child(fullscreen_button("fullscreen", self.fullscreen))
    }

    fn render_home(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(err) = &self.error {
            return div()
                .flex()
                .flex_col()
                .child(self.header("ztriton".into(), false, cx))
                .child(div().px_8().text_color(gpui::red()).child(format!("Error: {err}")))
                .into_any_element();
        }
        if self.jf.is_none() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(text_dim())
                .child(if self.generation == 0 { "Connecting to Jellyfin…" } else { "Signing out…" })
                .into_any_element();
        }

        let views = self.views.clone();
        let library_tabs = div().flex().gap_3().px_8().pb_2().children(views.into_iter().map(|v| {
            let vv = v.clone();
            div()
                .id(SharedString::from(format!("view-{}", v.id)))
                .px_4()
                .py_2()
                .rounded_full()
                .bg(surface_hi())
                .text_color(text())
                .cursor_pointer()
                .hover(|s| s.bg(accent()).text_color(bg()))
                .child(v.name.clone())
                .on_click(cx.listener(move |this, _, window, cx| this.open(vv.clone(), window, cx)))
        }));

        let shelves: Vec<(String, Vec<Item>, bool)> =
            self.shelves.iter().map(|s| (s.title.clone(), s.items.clone(), s.wide)).collect();
        let mut col = div()
            .id("home-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(self.header("ztriton".into(), false, cx))
            .child(library_tabs);
        for (i, (title, items, wide)) in shelves.into_iter().enumerate() {
            let cards: Vec<AnyElement> = items.iter().map(|it| self.poster_card(it, wide, cx)).collect();
            col = col.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .pt_6()
                    .child(div().px_8().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(text()).child(title))
                    .child(
                        div()
                            .id(("shelf", i))
                            .overflow_x_scroll()
                            .flex()
                            .gap_4()
                            .px_8()
                            .pb_2()
                            .children(cards),
                    ),
            );
        }
        col.child(div().h(px(40.))).into_any_element()
    }

    fn render_library(&mut self, parent: &Item, items: &Option<Arc<Vec<Item>>>, cx: &mut Context<Self>) -> AnyElement {
        let body = match items {
            None => div().px_8().text_color(text_dim()).child("Loading…").into_any_element(),
            Some(items) => {
                let wide = parent.kind == "Season";
                let cards: Vec<AnyElement> = items.iter().map(|it| self.poster_card(it, wide, cx)).collect();
                div().flex().flex_wrap().gap_6().px_8().children(cards).into_any_element()
            }
        };
        div()
            .id("lib-scroll")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .child(self.header(parent.name.clone(), true, cx))
            .child(body)
            .child(div().h(px(40.)))
            .into_any_element()
    }

    fn render_detail(&mut self, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        let backdrop_url = self.jf.as_ref().and_then(|jf| {
            jf.image_url(item, "Backdrop", 1920)
                .or_else(|| jf.image_url(item, "Thumb", 1920))
                .or_else(|| jf.image_url(item, "Primary", 1920))
        });
        let backdrop = self.image(backdrop_url, 1920, cx);
        let poster_url = self.jf.as_ref().and_then(|jf| jf.image_url(item, "Primary", 400));
        let poster = self.image(poster_url, 400, cx);

        let mut meta = vec![];
        if let Some(y) = item.production_year {
            meta.push(y.to_string());
        }
        if let Some(r) = &item.official_rating {
            meta.push(r.clone());
        }
        if let Some(rt) = item.runtime_secs() {
            meta.push(fmt_runtime(rt));
        }
        if let Some(c) = item.community_rating {
            meta.push(format!("★ {c:.1}"));
        }
        if !item.genres.is_empty() {
            meta.push(item.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
        }
        let resume = item.resume_secs();
        let it_play = item.clone();
        let it_start = item.clone();

        let button = |id: &'static str, label: String, primary: bool| {
            div()
                .id(id)
                .px_6()
                .py_3()
                .rounded_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .when(primary, |d| d.bg(accent()).text_color(bg()))
                .when(!primary, |d| d.bg(rgba(0xffffff26)).text_color(text()))
                .hover(|s| s.opacity(0.85))
                .child(label)
        };

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(match backdrop {
                Some(b) => img(b).absolute().top_0().left_0().size_full().object_fit(ObjectFit::Cover).into_any_element(),
                None => div().absolute().top_0().left_0().size_full().bg(surface()).into_any_element(),
            })
            .child(div().absolute().top_0().left_0().size_full().bg(linear_gradient(
                90.,
                linear_color_stop(rgba(0x0b0d12f5), 0.35),
                linear_color_stop(rgba(0x0b0d1240), 1.),
            )))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(self.header(String::new(), true, cx))
                    .child(
                        div()
                            .flex()
                            .gap_8()
                            .px_8()
                            .pt_8()
                            .child(
                                div()
                                    .w(px(220.))
                                    .h(px(330.))
                                    .flex_none()
                                    .rounded_xl()
                                    .overflow_hidden()
                                    .shadow_lg()
                                    .bg(surface_hi())
                                    .children(poster.map(|p| img(p).size_full().object_fit(ObjectFit::Cover))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_4()
                                    .max_w(px(640.))
                                    .child(div().text_3xl().font_weight(FontWeight::BOLD).text_color(text()).child(item.name.clone()))
                                    .when(item.kind == "Episode", |d| {
                                        d.child(div().text_lg().text_color(text_dim()).child(item.subtitle()))
                                    })
                                    .child(div().text_sm().text_color(text_dim()).child(meta.join("  ·  ")))
                                    .child(
                                        div()
                                            .flex()
                                            .gap_3()
                                            .pt_2()
                                            .child(
                                                button(
                                                    "play",
                                                    if resume > 5.0 { format!("▶  Resume from {}", fmt_time(resume)) } else { "▶  Play".into() },
                                                    true,
                                                )
                                                .on_click(cx.listener(move |this, _, window, cx| this.play(it_play.clone(), window, cx))),
                                            )
                                            .when(resume > 5.0, |d| {
                                                d.child(button("restart", "Play from Beginning".into(), false).on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        let mut i = it_start.clone();
                                                        if let Some(u) = i.user_data.as_mut() {
                                                            u.playback_position_ticks = 0;
                                                        }
                                                        this.play(i, window, cx)
                                                    },
                                                )))
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_color(text())
                                            .line_height(px(22.))
                                            .child(item.overview.clone().unwrap_or_default()),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for ZtritonApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ZtritonApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let screen = self.stack.last().cloned().unwrap_or(Screen::Home);
        let title = match &screen {
            _ if self.login.is_some() => "ztriton — Sign In".to_string(),
            Screen::Home => "ztriton".to_string(),
            Screen::Library { parent, .. } => format!("ztriton — {}", parent.name),
            Screen::Detail(i) => format!("ztriton — {}", i.name),
            Screen::Player(_) => "ztriton — Playing".to_string(),
        };
        self.fullscreen = window.is_fullscreen();
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }
        let content = match (&self.login, &screen) {
            (Some(login), _) => login.clone().into_any_element(),
            (None, Screen::Home) => self.render_home(cx),
            (None, Screen::Library { parent, items }) => self.render_library(parent, items, cx),
            (None, Screen::Detail(item)) => self.render_detail(item, cx),
            (None, Screen::Player(view)) => view.clone().into_any_element(),
        };
        // Login and the connecting/signing-out placeholder render no header, so
        // they get the fullscreen control as an overlay instead.
        let needs_overlay = self.login.is_some() || (self.jf.is_none() && self.error.is_none());
        div()
            .key_context("Ztriton")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::sign_out))
            // Handled on the root view rather than via a global `cx.on_action`: during key
            // dispatch the window is already leased, so updating it from a global handler fails.
            .on_action(|_: &ToggleFullscreen, window, _| window.toggle_fullscreen())
            .size_full()
            .bg(bg())
            .text_color(text())
            .font_family(".SystemUIFont")
            .relative()
            .child(content)
            .when(needs_overlay, |d| {
                d.child(div().absolute().top_8().right_8().child(fullscreen_button("fullscreen-overlay", self.fullscreen)))
            })
    }
}

/// Pixel width to request for an image shown `points` wide on a 2x display.
fn retina_pixels(points: f32) -> u32 {
    (points * 2.0).round() as u32
}
