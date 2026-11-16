//! Root view: navigation stack (Home -> Library -> Detail -> Player, and Search from Home or Library).

use super::login::{LoggedIn, LoginView};
use super::player_view::PlayerView;
use super::settings_view::SettingsView;
use super::text_input::{self, TextInput};
use super::*;
use gpui::{
    AnyElement, App, Context, ElementId, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    ObjectFit, Render, SharedString, Task, Window, actions, div, img, linear_color_stop,
    linear_gradient, prelude::*, px, rgba,
};
use std::collections::HashMap;
use std::time::Duration;
use ztriton::jellyfin::{Item, Jellyfin, SearchResults};
use ztriton::keychain::{self, SavedLogin};
use ztriton::resume::ResumeStore;

actions!(ztriton, [Back, Quit, ToggleFullscreen, SignOut, OpenSearch, OpenSettings]);

#[derive(Clone)]
enum Screen {
    Home,
    Library { parent: Item, items: Option<Arc<Vec<Item>>> },
    Detail {
        item: Item,
        /// Where Play resumes, in seconds: the newer of this Mac's and the server's position.
        resume_secs: f64,
    },
    Player(Entity<PlayerView>),
    Search(Entity<SearchState>),
    Settings(Entity<SettingsView>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchFilter {
    All,
    Movies,
    Shows,
    Episodes,
}

impl SearchFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Movies => "Movies",
            Self::Shows => "Shows",
            Self::Episodes => "Episodes",
        }
    }

    fn count(self, results: &SearchResults) -> usize {
        match self {
            Self::All => results.len(),
            Self::Movies => results.movies.len(),
            Self::Shows => results.shows.len(),
            Self::Episodes => results.episodes.len(),
        }
    }
}

/// The results shown on a Search screen and the query they answer.
struct ShownResults {
    query: String,
    results: Arc<SearchResults>,
}

struct SearchState {
    input: Entity<TextInput>,
    filter: SearchFilter,
    /// Trimmed query of the latest search started, so edits that only change
    /// surrounding whitespace do not search again.
    query: String,
    /// Kept while a newer query is in flight so the screen does not flicker.
    results: Option<ShownResults>,
    searching: bool,
    error: Option<String>,
    /// Replacing it drops the previous debounce or request, so only the latest
    /// query can fill the results.
    pending: Option<Task<()>>,
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
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const SEARCH_LIMIT: usize = 100;

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
        self.release_images(window);
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
        self.release_images(window);
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
                this.push_detail(item.clone());
                cx.notify();
            })
            .log_err();
            cx.background_executor().timer(Duration::from_secs(4)).await;
            cx.update(|cx| {
                if let Some(window) = cx.windows().first().copied() {
                    window
                        .update(cx, |_, window, cx| {
                            this.update(cx, |this, cx| {
                                let start = this.start_position(&item);
                                this.play(item, start, window, cx)
                            })
                            .log_err();
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
                // The cache may have been released meanwhile (player opened,
                // signed out); don't refill it behind its back.
                let Some(slot @ ImgState::Loading) = this.images.get_mut(&url) else { return };
                *slot = match res {
                    Ok(image) => ImgState::Ready(image),
                    Err(e) => {
                        log::warn!("image {}: {e:#}", ztriton::jellyfin::redact(&url));
                        ImgState::Failed
                    }
                };
                cx.notify();
            })
            .log_err();
        })
        .detach();
        None
    }

    /// Free every decoded image, both the CPU copy and its GPU atlas tile.
    /// Screens re-fetch what they show the next time they render.
    fn release_images(&mut self, window: &mut Window) {
        for (_, state) in self.images.drain() {
            if let ImgState::Ready(image) = state {
                window.drop_image(image).log_err();
            }
        }
    }

    fn open(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
        if item.is_playable() {
            self.push_detail(item);
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
        // A card clicked on the Search screen leaves the field focused, and its
        // focus handle is not rendered on the next screen.
        window.focus(&self.focus);
        cx.notify();
    }

    fn open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.login.is_some() || self.jf.is_none() {
            return;
        }
        match self.stack.last() {
            Some(Screen::Player(_)) => return,
            Some(Screen::Search(search)) => {
                let input = search.read(cx).input.clone();
                window.focus(&input.focus_handle(cx));
                input.update(cx, |input, cx| input.select_all_text(cx));
                return;
            }
            _ => {}
        }
        let input = cx.new(|cx| TextInput::new("Movies, shows and episodes", false, cx).without_frame());
        // The subscription ends when the field is released with its Search screen.
        cx.subscribe(&input, |this, input, _: &text_input::Changed, cx| this.search_query_changed(&input, cx))
            .detach();
        window.focus(&input.focus_handle(cx));
        let search = cx.new(|_| SearchState {
            input,
            filter: SearchFilter::All,
            query: String::new(),
            results: None,
            searching: false,
            error: None,
            pending: None,
        });
        self.stack.push(Screen::Search(search));
        cx.notify();
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if self.login.is_some() || self.jf.is_none() {
            return;
        }
        if matches!(self.stack.last(), Some(Screen::Player(_))) {
            return;
        }
        // One Settings screen at most: a second copy would save over the first's changes.
        if let Some(index) = self.stack.iter().position(|screen| matches!(screen, Screen::Settings(_))) {
            self.stack.truncate(index + 1);
            window.focus(&self.focus);
            cx.notify();
            return;
        }
        let settings = cx.new(SettingsView::new);
        window.focus(&self.focus);
        self.stack.push(Screen::Settings(settings));
        cx.notify();
    }

    fn search_query_changed(&mut self, input: &Entity<TextInput>, cx: &mut Context<Self>) {
        let search = self.stack.iter().find_map(|screen| match screen {
            Screen::Search(search) if search.read(cx).input == *input => Some(search.clone()),
            _ => None,
        });
        let (Some(search), Some(jf)) = (search, self.jf.clone()) else { return };
        let query = input.read(cx).text().trim().to_string();
        if query == search.read(cx).query {
            return;
        }
        if query.is_empty() {
            search.update(cx, |state, _| {
                state.query.clear();
                state.pending = None;
                state.results = None;
                state.searching = false;
                state.error = None;
            });
            cx.notify();
            return;
        }
        let weak_search = search.downgrade();
        let task = cx.spawn({
            let query = query.clone();
            async move |this, cx| {
                cx.background_executor().timer(SEARCH_DEBOUNCE).await;
                let result = cx
                    .background_executor()
                    .spawn({
                        let query = query.clone();
                        async move { jf.search(&query, SEARCH_LIMIT) }
                    })
                    .await;
                this.update(cx, |_, cx| {
                    let Some(search) = weak_search.upgrade() else { return };
                    search.update(cx, |state, _| {
                        state.searching = false;
                        match result {
                            Ok(results) => {
                                proof_log(format!(
                                    "search: '{query}' -> {} movies, {} shows, {} episodes",
                                    results.movies.len(),
                                    results.shows.len(),
                                    results.episodes.len()
                                ));
                                if state.filter.count(&results) == 0 {
                                    state.filter = SearchFilter::All;
                                }
                                state.results = Some(ShownResults { query, results: Arc::new(results) });
                                state.error = None;
                            }
                            Err(e) => {
                                proof_log(format!("search: '{query}' failed: {e:#}"));
                                state.results = None;
                                state.error = Some(format!("{e:#}"));
                            }
                        }
                    });
                    cx.notify();
                })
                .log_err();
            }
        });
        search.update(cx, |state, _| {
            state.query = query;
            state.searching = true;
            state.pending = Some(task);
        });
        cx.notify();
    }

    fn start_position(&self, item: &Item) -> f64 {
        let Some(jf) = &self.jf else { return item.resume_secs() };
        match ResumeStore::load() {
            Ok(store) => store.start_position(&jf.user_id, item),
            Err(e) => {
                log::warn!("could not read resume positions: {e:#}");
                item.resume_secs()
            }
        }
    }

    fn push_detail(&mut self, item: Item) {
        let resume_secs = self.start_position(&item);
        self.stack.push(Screen::Detail { item, resume_secs });
    }

    /// Re-read the item from the server after playback so the Detail screen
    /// shows where Play now resumes (and whether it was watched).
    fn refresh_detail(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(jf) = self.jf.clone() else { return };
        cx.spawn(async move |this, cx| {
            let item = cx.background_executor().spawn(async move { jf.item(&item_id) }).await;
            this.update(cx, |this, cx| {
                let item = match item {
                    Ok(item) => item,
                    Err(e) => {
                        log::warn!("could not refresh the played item: {e:#}");
                        return;
                    }
                };
                let resume = this.start_position(&item);
                if let Some(Screen::Detail { item: shown, resume_secs }) = this.stack.last_mut()
                    && shown.id == item.id
                {
                    *shown = item;
                    *resume_secs = resume;
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    fn play(&mut self, item: Item, start: f64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(jf) = self.jf.clone() else { return };
        // Posters and backdrops are hidden for the whole playback; holding them
        // would keep tens of MB of RAM and GPU memory busy.
        self.release_images(window);
        log::info!("playing '{}' ({}) from {start:.2}s", item.name, item.id);
        let item_id = item.id.clone();
        let view = cx.new(|cx| PlayerView::new(jf, item, start, window, cx));
        cx.subscribe_in(&view, window, {
            let item_id = item_id.clone();
            move |this, _, super::player_view::PlayEpisode(episode), window, cx| {
                if matches!(this.stack.last(), Some(Screen::Player(_))) {
                    drop(this.stack.pop());
                }
                // Esc after switching returns to the episode last played, not the one first opened.
                if matches!(this.stack.last(), Some(Screen::Detail { item, .. }) if item.id == item_id) {
                    drop(this.stack.pop());
                    this.push_detail(episode.clone());
                }
                let start = this.start_position(episode);
                this.play(episode.clone(), start, window, cx);
            }
        })
        .detach();
        cx.subscribe_in(&view, window, move |this, _, _: &super::player_view::Closed, window, cx| {
            if matches!(this.stack.last(), Some(Screen::Player(_))) {
                this.pop(cx);
            }
            // The player saved its position before emitting Closed; show it now
            // rather than only once (and if) the server answers.
            let local = match this.stack.last() {
                Some(Screen::Detail { item, .. }) if item.id == item_id => Some(this.start_position(item)),
                _ => None,
            };
            if let (Some(resume), Some(Screen::Detail { resume_secs, .. })) = (local, this.stack.last_mut()) {
                *resume_secs = resume;
            }
            this.refresh_detail(item_id.clone(), cx);
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

    fn back_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("back")
            .flex_none()
            .px_3()
            .py_1()
            .rounded_md()
            .bg(surface_hi())
            .cursor_pointer()
            .hover(|s| s.bg(accent()).text_color(bg()))
            .child("‹ Back")
            .on_click(cx.listener(|this, _, window, cx| this.back(&Back, window, cx)))
    }

    fn header(&self, title: String, can_back: bool, searchable: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_4()
            .px_8()
            .pt_8()
            .pb_4()
            .when(can_back, |d| d.child(self.back_button(cx)))
            .child(div().text_3xl().font_weight(FontWeight::BOLD).text_color(text()).child(title))
            .child(div().flex_1())
            .when(searchable && self.jf.is_some(), |d| {
                d.child(
                    div()
                        .id("search")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(surface_hi())
                        .text_sm()
                        .text_color(text())
                        .cursor_pointer()
                        .hover(|s| s.bg(accent()).text_color(bg()))
                        .child(magnifier_icon(14.))
                        .child("Search")
                        .child(div().text_xs().opacity(0.7).child("⌘F"))
                        .on_click(cx.listener(|this, _, window, cx| this.open_search(&OpenSearch, window, cx))),
                )
            })
            .when(!can_back && self.jf.is_some(), |d| {
                d.child(
                    div()
                        .id("settings")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(surface_hi())
                        .text_sm()
                        .text_color(text_dim())
                        .cursor_pointer()
                        .hover(|s| s.bg(accent()).text_color(bg()))
                        .child("Settings")
                        .on_click(cx.listener(|this, _, window, cx| this.open_settings(&OpenSettings, window, cx))),
                )
                .child(
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
                .child(self.header("ztriton".into(), false, true, cx))
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
            .child(self.header("ztriton".into(), false, true, cx))
            .child(library_tabs);
        for (i, (title, items, wide)) in shelves.into_iter().enumerate() {
            let cards: Vec<AnyElement> = items.iter().map(|it| self.poster_card(it, wide, cx)).collect();
            col = col.child(shelf(("shelf", i).into(), title, cards));
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
            .child(self.header(parent.name.clone(), true, true, cx))
            .child(body)
            .child(div().h(px(40.)))
            .into_any_element()
    }

    fn render_search(&mut self, search: &Entity<SearchState>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let state = search.read(cx);
        let input = state.input.clone();
        let filter = state.filter;
        let searching = state.searching;
        let error = state.error.clone();
        let has_query = !state.query.is_empty();
        let shown = state.results.as_ref().map(|shown| (shown.query.clone(), shown.results.clone()));
        let input_focused = input.focus_handle(cx).is_focused(window);
        let input_empty = input.read(cx).text().is_empty();

        let field = div()
            .id("search-field")
            .flex_1()
            .max_w(px(720.))
            .h(px(44.))
            .flex()
            .items_center()
            .gap(px(10.))
            .px_3()
            .rounded(px(10.))
            .bg(surface_hi())
            .border_1()
            .border_color(if input_focused { accent() } else { rgba(0xffffff1a).into() })
            .text_color(text_dim())
            .cursor_text()
            .on_click({
                let input = input.clone();
                move |_, window, cx| window.focus(&input.focus_handle(cx))
            })
            .child(magnifier_icon(16.))
            .child(div().flex_1().min_w_0().child(input.clone()))
            .when(!input_empty, |d| {
                d.child(
                    div()
                        .id("search-clear")
                        .flex_none()
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(rgba(0x3a4252ff))
                        .text_xs()
                        .text_color(text())
                        .cursor_pointer()
                        .hover(|s| s.bg(accent()).text_color(bg()))
                        .child("✕")
                        .on_click({
                            let input = input.clone();
                            move |_, window, cx| {
                                input.update(cx, |input, cx| input.set_text("", cx));
                                window.focus(&input.focus_handle(cx));
                            }
                        }),
                )
            });
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_4()
            .px_8()
            .pt_8()
            .pb_4()
            .child(self.back_button(cx))
            .child(field)
            .child(div().flex_1())
            .child(fullscreen_button("fullscreen", self.fullscreen));

        let status = if searching {
            Some("Searching…".to_string())
        } else {
            shown.as_ref().map(|(_, results)| match results.len() {
                1 => "1 result".to_string(),
                count => format!("{count} results"),
            })
        };
        let pills = shown.as_ref().filter(|_| has_query).map(|(_, results)| {
            [SearchFilter::All, SearchFilter::Movies, SearchFilter::Shows, SearchFilter::Episodes]
                .into_iter()
                .filter(|pill| *pill == SearchFilter::All || pill.count(results) > 0)
                .map(|pill| {
                    let selected = pill == filter;
                    let search = search.clone();
                    div()
                        .id(SharedString::from(format!("filter-{}", pill.label())))
                        .flex()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .rounded_full()
                        .text_sm()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(accent()).text_color(bg()).font_weight(FontWeight::SEMIBOLD))
                        .when(!selected, |d| d.bg(surface_hi()).text_color(text()).hover(|s| s.bg(accent()).text_color(bg())))
                        .child(pill.label())
                        .child(div().opacity(0.65).child(pill.count(results).to_string()))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            search.update(cx, |state, _| state.filter = pill);
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        });
        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px_8()
            .pt_1()
            .pb_2()
            .children(pills.unwrap_or_default())
            .child(div().flex_1())
            .children(status.filter(|_| has_query).map(|status| div().text_sm().text_color(text_dim()).child(status)));

        let message = |message: String| div().px_8().pt_6().text_color(text_dim()).child(message).into_any_element();
        let body = match (&error, &shown) {
            _ if !has_query => message("Search your movies, shows and episodes".into()),
            (Some(error), _) => div().px_8().pt_6().text_color(gpui::red()).child(format!("Error: {error}")).into_any_element(),
            (None, None) => div().into_any_element(),
            (None, Some((query, results))) if results.is_empty() => message(format!("No results for “{query}”")),
            (None, Some((_, results))) => {
                let sections = [
                    (SearchFilter::Movies, &results.movies, false),
                    (SearchFilter::Shows, &results.shows, false),
                    (SearchFilter::Episodes, &results.episodes, true),
                ];
                let mut column = div().flex().flex_col();
                for (section, items, wide) in sections {
                    if items.is_empty() || (filter != SearchFilter::All && filter != section) {
                        continue;
                    }
                    let cards: Vec<AnyElement> = items.iter().map(|item| self.poster_card(item, wide, cx)).collect();
                    column = column.child(if filter == SearchFilter::All {
                        shelf(SharedString::from(format!("search-{}", section.label())).into(), section.label(), cards)
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .pt_6()
                            .child(section_title(section.label()))
                            .child(div().flex().flex_wrap().gap_6().px_8().children(cards))
                            .into_any_element()
                    });
                }
                column.into_any_element()
            }
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(toolbar)
            .child(
                div()
                    .id("search-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body)
                    .child(div().h(px(40.))),
            )
            .into_any_element()
    }

    fn render_detail(&mut self, item: &Item, resume: f64, cx: &mut Context<Self>) -> AnyElement {
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
                    .child(self.header(String::new(), true, false, cx))
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
                                                .on_click(cx.listener(move |this, _, window, cx| {
                                                    let start = this.start_position(&it_play);
                                                    this.play(it_play.clone(), start, window, cx)
                                                })),
                                            )
                                            .when(resume > 5.0, |d| {
                                                d.child(button("restart", "Play from Beginning".into(), false).on_click(cx.listener(
                                                    move |this, _, window, cx| this.play(it_start.clone(), 0.0, window, cx),
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
            Screen::Detail { item, .. } => format!("ztriton — {}", item.name),
            Screen::Player(_) => "ztriton — Playing".to_string(),
            Screen::Search(_) => "ztriton — Search".to_string(),
            Screen::Settings(_) => "ztriton — Settings".to_string(),
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
            (None, Screen::Detail { item, resume_secs }) => self.render_detail(item, *resume_secs, cx),
            (None, Screen::Player(view)) => view.clone().into_any_element(),
            (None, Screen::Search(search)) => self.render_search(search, window, cx),
            (None, Screen::Settings(settings)) => div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.header("Settings".into(), true, false, cx))
                .child(div().flex_1().min_h_0().child(settings.clone()))
                .into_any_element(),
        };
        // Login and the connecting/signing-out placeholder render no header, so
        // they get the fullscreen control as an overlay instead.
        let needs_overlay = self.login.is_some() || (self.jf.is_none() && self.error.is_none());
        div()
            .key_context("Ztriton")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::sign_out))
            .on_action(cx.listener(Self::open_search))
            .on_action(cx.listener(Self::open_settings))
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

fn section_title(title: impl Into<SharedString>) -> impl IntoElement {
    div().px_8().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(text()).child(title.into())
}

/// A titled, horizontally scrolling row of cards.
fn shelf(id: ElementId, title: impl Into<SharedString>, cards: Vec<AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_6()
        .child(section_title(title))
        .child(div().id(id).overflow_x_scroll().flex().gap_4().px_8().pb_2().children(cards))
}

/// Pixel width to request for an image shown `points` wide on a 2x display.
fn retina_pixels(points: f32) -> u32 {
    (points * 2.0).round() as u32
}
