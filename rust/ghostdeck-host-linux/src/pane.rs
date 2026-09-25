//! PaneWidget: a tabbed container with action icons in the tab bar.
//!
//! Layout: [tab1 x] [tab2 x] ... ←spacer→ [terminal] [split-h] [split-v] [close]
//!
//! All on one line. Tabs left-justified, icons right-justified.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use gtk::glib;
#[allow(unused_imports)]
use gtk::prelude::*;
use gtk4 as gtk;

use crate::app_config::AppConfig;
use crate::keybind_editor;
use crate::layout_state::{
    self, PaneState, RestorableAgentState, TabContentState, TabState as SavedTabState,
};
use crate::settings_editor;
use crate::shortcut_config::{NormalizedShortcut, ResolvedShortcutConfig, ShortcutId};
use crate::terminal::{self, TerminalCallbacks};
use crate::window;

static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(1);

fn next_pane_id() -> u32 {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

fn reserve_pane_id(id: u32) {
    let mut current = NEXT_PANE_ID.load(Ordering::Relaxed);
    while current <= id {
        match NEXT_PANE_ID.compare_exchange_weak(
            current,
            id.saturating_add(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(updated) => current = updated,
        }
    }
}

fn pane_id_for_initial_state(initial_state: Option<&PaneState>) -> u32 {
    if let Some(id) = initial_state
        .and_then(|state| state.pane_id)
        .filter(|id| *id > 0)
    {
        reserve_pane_id(id);
        return id;
    }
    next_pane_id()
}

type TabDragCallback = dyn Fn(bool);

thread_local! {
    static TAB_DRAGGING: Cell<bool> = const { Cell::new(false) };
    static TAB_DRAG_LISTENERS: RefCell<std::collections::HashMap<usize, Box<TabDragCallback>>> =
        RefCell::new(std::collections::HashMap::new());
    static TAB_DRAG_NEXT_ID: Cell<usize> = const { Cell::new(1) };
    static PANE_REGISTRY: RefCell<std::collections::HashMap<u32, std::rc::Weak<PaneInternals>>> =
        RefCell::new(std::collections::HashMap::new());
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TabDragPayload {
    pane_id: u32,
    tab_id: String,
}

impl TabDragPayload {
    fn new(pane_id: u32, tab_id: impl Into<String>) -> Self {
        Self {
            pane_id,
            tab_id: tab_id.into(),
        }
    }

    fn encode(&self) -> String {
        format!("{}:{}", self.pane_id, self.tab_id)
    }

    fn decode(raw: &str) -> Option<Self> {
        let (pane_id, tab_id) = raw.split_once(':')?;
        if tab_id.is_empty() {
            return None;
        }
        Some(Self::new(pane_id.parse::<u32>().ok()?, tab_id))
    }
}

const HOST_ENTRY_CSS_CLASS: &str = "ghostdeck-host-entry";
const TAB_RENAME_ENTRY_CSS_CLASS: &str = "ghostdeck-tab-rename-entry";
const TAB_RENAME_ENTRY_CSS_CLASSES: [&str; 2] = [HOST_ENTRY_CSS_CLASS, TAB_RENAME_ENTRY_CSS_CLASS];
pub(crate) const MIN_PANE_WIDTH: i32 = 260;
pub(crate) const MIN_PANE_HEIGHT: i32 = 160;

pub fn is_tab_dragging() -> bool {
    TAB_DRAGGING.with(|value| value.get())
}

pub fn on_tab_drag_change(callback: impl Fn(bool) + 'static) -> usize {
    TAB_DRAG_LISTENERS.with(|listeners| {
        let id = TAB_DRAG_NEXT_ID.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        listeners.borrow_mut().insert(id, Box::new(callback));
        id
    })
}

fn set_tab_dragging(active: bool) {
    TAB_DRAGGING.with(|value| value.set(active));
    TAB_DRAG_LISTENERS.with(|listeners| {
        for callback in listeners.borrow().values() {
            callback(active);
        }
    });
}

fn register_pane(id: u32, internals: &Rc<PaneInternals>) {
    PANE_REGISTRY.with(|registry| {
        registry.borrow_mut().insert(id, Rc::downgrade(internals));
    });
}

fn unregister_pane(id: u32) {
    PANE_REGISTRY.with(|registry| {
        registry.borrow_mut().remove(&id);
    });
}

fn lookup_pane_internals(id: u32) -> Option<Rc<PaneInternals>> {
    PANE_REGISTRY.with(|registry| registry.borrow().get(&id)?.upgrade())
}

pub fn find_pane_widget_by_id(pane_id: u32) -> Option<gtk::Widget> {
    lookup_pane_internals(pane_id).map(|internals| internals.pane_outer.clone().upcast())
}

pub fn close_processes(pane_widget: &gtk::Widget) {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return;
    };
    for entry in &internals.tab_state.borrow().tabs {
        if let TabKind::Terminal { state } = &entry.kind {
            state
                .inner
                .tree
                .borrow()
                .for_each_leaf(|leaf| leaf.handle.close());
        }
    }
}

pub fn set_workspace_dragging_all(active: bool) {
    PANE_REGISTRY.with(|registry| {
        for weak in registry.borrow().values() {
            if let Some(internals) = weak.upgrade() {
                internals.workspace_dragging.set(active);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type PaneSignalCallback = dyn Fn();
type PaneBellCallback = dyn Fn(bool, u32, &str);
type PanePathCallback = dyn Fn(&str);
type PaneDesktopNotificationCallback = dyn Fn(&str, &str, bool, u32, &str);
type PaneEmptyCallback = dyn Fn();
type PaneShortcutStateCallback = dyn Fn() -> Rc<ResolvedShortcutConfig>;
type PaneShortcutCaptureCallback =
    dyn Fn(ShortcutId, Option<NormalizedShortcut>) -> Result<ResolvedShortcutConfig, String>;
type PaneConfigCallback = dyn Fn() -> Rc<RefCell<AppConfig>>;
type PaneConfigChangedCallback = dyn Fn(&AppConfig, &AppConfig);
/// Returns the workspace id that owns a given pane widget, or `None` if the
/// pane is not yet attached to a workspace. Used to stamp `GHOSTDECK_WORKSPACE_ID`
/// onto every terminal spawned inside the pane.
type PaneWorkspaceLookupCallback = dyn Fn(&gtk::Widget) -> Option<String>;

pub struct PaneCallbacks {
    pub on_bell: Box<PaneBellCallback>,
    pub on_desktop_notification: Box<PaneDesktopNotificationCallback>,
    pub current_shortcuts: Box<PaneShortcutStateCallback>,
    pub on_capture_shortcut: Rc<PaneShortcutCaptureCallback>,
    pub on_pwd_changed: Box<PanePathCallback>,
    pub on_empty: Box<PaneEmptyCallback>,
    pub on_state_changed: Box<PaneSignalCallback>,
    pub current_config: Box<PaneConfigCallback>,
    pub on_config_changed: Rc<PaneConfigChangedCallback>,
    /// Resolve the workspace id for a given pane widget. May be `None` while
    /// the pane is still being constructed; callers treat that as "unknown".
    pub workspace_for_pane: Box<PaneWorkspaceLookupCallback>,
}

#[derive(Clone)]
struct TerminalTabState {
    inner: Rc<TerminalTabInner>,
}

struct TerminalTabInner {
    tree: RefCell<TerminalSplitNode>,
    active_leaf_id: RefCell<String>,
    root: gtk::Box,
    resize_overlay: gtk::Overlay,
    rebuild_source: RefCell<Option<glib::SourceId>>,
    focus_after_rebuild: Cell<bool>,
    swap_source: RefCell<Option<String>>,
    swap_buttons: RefCell<Vec<(String, gtk::Button)>>,
    active_resize_split: Cell<usize>,
    resize_revealers: RefCell<Vec<gtk::Revealer>>,
    resize_layer: gtk::Fixed,
    on_state_changed: RefCell<Option<std::rc::Weak<PaneCallbacks>>>,
}

#[derive(Clone)]
struct TerminalLeafState {
    leaf_id: String,
    surface_id: String,
    creator_surface_id: Option<String>,
    cwd: Rc<RefCell<Option<String>>>,
    agent: Rc<RefCell<Option<RestorableAgentState>>>,
    handle: terminal::TerminalHandle,
    widget: gtk::Widget,
}

#[derive(Clone)]
enum TerminalSplitNode {
    Leaf(TerminalLeafState),
    Split {
        orientation: gtk::Orientation,
        ratio: Rc<RefCell<f64>>,
        start: Box<TerminalSplitNode>,
        end: Box<TerminalSplitNode>,
    },
}

impl TerminalSplitNode {
    fn first_leaf(&self) -> &TerminalLeafState {
        match self {
            Self::Leaf(leaf) => leaf,
            Self::Split { start, .. } => start.first_leaf(),
        }
    }

    fn find_leaf(&self, leaf_id: &str) -> Option<&TerminalLeafState> {
        match self {
            Self::Leaf(leaf) => (leaf.leaf_id == leaf_id).then_some(leaf),
            Self::Split { start, end, .. } => {
                start.find_leaf(leaf_id).or_else(|| end.find_leaf(leaf_id))
            }
        }
    }

    fn find_surface(
        &self,
        pane_id: u32,
        tab_id: &str,
        surface_hint: &str,
    ) -> Option<&TerminalLeafState> {
        match self {
            Self::Leaf(leaf)
                if terminal_surface_id(pane_id, tab_id, &leaf.leaf_id)
                    == normalize_surface_hint(surface_hint) =>
            {
                Some(leaf)
            }
            Self::Leaf(_) => None,
            Self::Split { start, end, .. } => start
                .find_surface(pane_id, tab_id, surface_hint)
                .or_else(|| end.find_surface(pane_id, tab_id, surface_hint)),
        }
    }

    fn for_each_leaf(&self, mut visit: impl FnMut(&TerminalLeafState)) {
        fn walk(node: &TerminalSplitNode, visit: &mut dyn FnMut(&TerminalLeafState)) {
            match node {
                TerminalSplitNode::Leaf(leaf) => visit(leaf),
                TerminalSplitNode::Split { start, end, .. } => {
                    walk(start, visit);
                    walk(end, visit);
                }
            }
        }

        walk(self, &mut visit);
    }

    fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { start, end, .. } => start.leaf_count() + end.leaf_count(),
        }
    }

    fn replace_leaf(&mut self, leaf_id: &str, replacement: TerminalSplitNode) -> bool {
        match self {
            Self::Leaf(leaf) => {
                if leaf.leaf_id == leaf_id {
                    *self = replacement;
                    true
                } else {
                    false
                }
            }
            Self::Split { start, end, .. } => {
                start.replace_leaf(leaf_id, replacement.clone())
                    || end.replace_leaf(leaf_id, replacement)
            }
        }
    }

    fn remove_leaf(&mut self, leaf_id: &str) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { start, end, .. } => {
                if matches!(start.as_ref(), Self::Leaf(leaf) if leaf.leaf_id == leaf_id) {
                    *self = std::mem::replace(end.as_mut(), Self::Leaf(start.first_leaf().clone()));
                    return true;
                }
                if matches!(end.as_ref(), Self::Leaf(leaf) if leaf.leaf_id == leaf_id) {
                    *self = std::mem::replace(start.as_mut(), Self::Leaf(end.first_leaf().clone()));
                    return true;
                }
                start.remove_leaf(leaf_id) || end.remove_leaf(leaf_id)
            }
        }
    }

    fn snapshot(&self) -> layout_state::TerminalTreeState {
        match self {
            Self::Leaf(leaf) => {
                if let Some(cwd) = crate::process_cwd::surface_cwd(&leaf.surface_id) {
                    *leaf.cwd.borrow_mut() = Some(cwd);
                }
                layout_state::TerminalTreeState::Leaf(Box::new(layout_state::TerminalLeafState {
                    leaf_id: Some(leaf.leaf_id.clone()),
                    creator_surface_id: leaf.creator_surface_id.clone(),
                    cwd: leaf.cwd.borrow().clone(),
                    agent: leaf.agent.borrow().clone(),
                }))
            }
            Self::Split {
                orientation,
                ratio,
                start,
                end,
            } => layout_state::TerminalTreeState::Split(layout_state::TerminalSplitState {
                orientation: if *orientation == gtk::Orientation::Horizontal {
                    layout_state::SplitOrientation::Horizontal
                } else {
                    layout_state::SplitOrientation::Vertical
                },
                ratio: *ratio.borrow(),
                start: Box::new(start.snapshot()),
                end: Box::new(end.snapshot()),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalFocusDirection {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalTabSurfaceError {
    PaneNotFound,
    TabNotFound,
    NotTerminal,
    SourceNotFound,
    SurfaceNotFound,
    SurfaceNotOwned,
    CannotCloseSource,
    CommandNotWritable,
    UnsupportedLayout,
    LimitReached,
}

impl TerminalTabState {
    fn from_tree(tree: TerminalSplitNode, active_leaf_id: Option<String>) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        let resize_layer = gtk::Fixed::new();
        resize_layer.set_hexpand(true);
        resize_layer.set_vexpand(true);
        resize_layer.set_halign(gtk::Align::Fill);
        resize_layer.set_valign(gtk::Align::Fill);
        resize_layer.set_can_target(false);
        let resize_overlay = gtk::Overlay::new();
        resize_overlay.add_overlay(&resize_layer);
        let active_leaf_id = active_leaf_id.unwrap_or_else(|| tree.first_leaf().leaf_id.clone());
        let state = Self {
            inner: Rc::new(TerminalTabInner {
                tree: RefCell::new(tree),
                active_leaf_id: RefCell::new(active_leaf_id),
                root,
                resize_overlay,
                rebuild_source: RefCell::new(None),
                focus_after_rebuild: Cell::new(false),
                swap_source: RefCell::new(None),
                swap_buttons: RefCell::new(Vec::new()),
                active_resize_split: Cell::new(0),
                resize_revealers: RefCell::new(Vec::new()),
                resize_layer,
                on_state_changed: RefCell::new(None),
            }),
        };
        state
            .inner
            .resize_overlay
            .set_child(Some(&build_terminal_split_widget_tree(
                &state.inner.tree.borrow(),
                &state,
            )));
        state.inner.root.append(&state.inner.resize_overlay);
        let release = gtk::EventControllerLegacy::new();
        release.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&state.inner);
        release.connect_event(move |_, event| {
            if matches!(
                event.event_type(),
                gtk::gdk::EventType::ButtonRelease | gtk::gdk::EventType::GrabBroken
            ) {
                if let Some(inner) = weak.upgrade() {
                    inner.active_resize_split.set(0);
                }
            }
            glib::Propagation::Proceed
        });
        state.inner.root.add_controller(release);
        let key_controller = gtk::EventControllerKey::new();
        key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&state.inner);
        key_controller.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(inner) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let state = TerminalTabState { inner };
            if state.inner.swap_source.borrow().is_none() {
                return glib::Propagation::Proceed;
            }
            if key == gtk::gdk::Key::Escape {
                state.cancel_swap();
            } else if modifiers.is_empty() {
                let index = key.to_unicode().and_then(|digit| digit.to_digit(10));
                if let Some(target) = index.and_then(|index| {
                    state
                        .inner
                        .swap_buttons
                        .borrow()
                        .get(index.checked_sub(1)? as usize)
                        .map(|(id, _)| id.clone())
                }) {
                    state.finish_swap(&target);
                } else {
                    return glib::Propagation::Proceed;
                }
            } else {
                return glib::Propagation::Proceed;
            }
            glib::Propagation::Stop
        });
        state.inner.root.add_controller(key_controller);
        state.sync_split_dimming();
        state
    }
    fn root(&self) -> gtk::Widget {
        self.inner.root.clone().upcast()
    }

    fn active_leaf(&self) -> TerminalLeafState {
        let tree = self.inner.tree.borrow();
        tree.find_leaf(&self.inner.active_leaf_id.borrow())
            .cloned()
            .unwrap_or_else(|| tree.first_leaf().clone())
    }

    fn active_leaf_id(&self) -> String {
        self.active_leaf().leaf_id
    }

    fn set_active_leaf(&self, leaf_id: &str) -> bool {
        let tree = self.inner.tree.borrow();
        if tree.find_leaf(leaf_id).is_none() || *self.inner.active_leaf_id.borrow() == leaf_id {
            return false;
        }
        drop(tree);
        *self.inner.active_leaf_id.borrow_mut() = leaf_id.to_string();
        self.sync_split_dimming();
        true
    }

    fn active_handle(&self) -> terminal::TerminalHandle {
        self.active_leaf().handle
    }

    fn has_leaf(&self, leaf_id: &str) -> bool {
        self.inner.tree.borrow().find_leaf(leaf_id).is_some()
    }

    fn active_cwd(&self) -> Option<String> {
        self.active_leaf().cwd.borrow().clone()
    }

    fn active_agent(&self) -> Option<RestorableAgentState> {
        self.active_leaf().agent.borrow().clone()
    }

    fn leaf_count(&self) -> usize {
        self.inner.tree.borrow().leaf_count()
    }

    fn start_swap(&self, source: &str) {
        let tree = self.inner.tree.borrow();
        let Some(handle) = tree.find_leaf(source).map(|leaf| leaf.handle.clone()) else {
            return;
        };
        if tree.leaf_count() < 2 {
            return;
        }
        drop(tree);
        *self.inner.swap_source.borrow_mut() = Some(source.to_string());
        for (index, (id, button)) in self.inner.swap_buttons.borrow().iter().enumerate() {
            let selected = id == source;
            button.set_label(&if selected {
                format!("{} · Selected", index + 1)
            } else {
                (index + 1).to_string()
            });
            button.set_visible(true);
            if selected {
                button.add_css_class("ghostdeck-swap-source");
            } else {
                button.remove_css_class("ghostdeck-swap-source");
            }
        }
        handle.focus_surface();
    }

    fn cancel_swap(&self) {
        self.inner.swap_source.borrow_mut().take();
        for (_, button) in self.inner.swap_buttons.borrow().iter() {
            button.set_visible(false);
        }
    }

    fn finish_swap(&self, target: &str) {
        let Some(source) = self.inner.swap_source.borrow_mut().take() else {
            return;
        };
        self.cancel_swap();
        if source == target {
            return;
        }
        let mut tree = self.inner.tree.borrow_mut();
        let (Some(source_leaf), Some(target_leaf)) = (
            tree.find_leaf(&source).cloned(),
            tree.find_leaf(target).cloned(),
        ) else {
            return;
        };
        let mut pending = vec![&mut *tree];
        while let Some(node) = pending.pop() {
            match node {
                TerminalSplitNode::Leaf(leaf) if leaf.leaf_id == source => {
                    *leaf = target_leaf.clone();
                }
                TerminalSplitNode::Leaf(leaf) if leaf.leaf_id == target => {
                    *leaf = source_leaf.clone();
                }
                TerminalSplitNode::Split { start, end, .. } => {
                    pending.push(start);
                    pending.push(end);
                }
                _ => {}
            }
        }
        drop(tree);
        *self.inner.active_leaf_id.borrow_mut() = source;
        self.sync_split_dimming();
        self.trigger_rebuild(true);
        if let Some(callbacks) = self
            .inner
            .on_state_changed
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade)
        {
            (callbacks.on_state_changed)();
        }
    }

    fn refresh_display(&self) {
        self.inner
            .tree
            .borrow()
            .for_each_leaf(|leaf| leaf.handle.refresh_display());
    }

    fn sync_split_dimming(&self) {
        let tree = self.inner.tree.borrow();
        let should_dim = tree.leaf_count() > 1;
        let active_leaf_id = self.inner.active_leaf_id.borrow().clone();
        tree.for_each_leaf(|leaf| {
            leaf.handle
                .set_split_dimmed(should_dim && leaf.leaf_id != active_leaf_id);
        });
    }

    fn replace_callbacks(&self, mut build: impl FnMut(&TerminalLeafState) -> TerminalCallbacks) {
        self.inner.tree.borrow().for_each_leaf(|leaf| {
            leaf.handle.replace_callbacks(build(leaf));
        });
    }

    fn snapshot_tree(&self) -> layout_state::TerminalTreeState {
        self.inner.tree.borrow().snapshot()
    }

    fn split_leaf(
        &self,
        leaf_id: &str,
        new_leaf: TerminalLeafState,
        orientation: gtk::Orientation,
        new_leaf_first: bool,
    ) -> bool {
        let Some(target_leaf) = self.inner.tree.borrow().find_leaf(leaf_id).cloned() else {
            return false;
        };
        let ratio = Rc::new(RefCell::new(layout_state::DEFAULT_SPLIT_RATIO));
        let replaced = self.inner.tree.borrow_mut().replace_leaf(
            leaf_id,
            if new_leaf_first {
                TerminalSplitNode::Split {
                    orientation,
                    ratio,
                    start: Box::new(TerminalSplitNode::Leaf(new_leaf.clone())),
                    end: Box::new(TerminalSplitNode::Leaf(target_leaf)),
                }
            } else {
                TerminalSplitNode::Split {
                    orientation,
                    ratio,
                    start: Box::new(TerminalSplitNode::Leaf(target_leaf)),
                    end: Box::new(TerminalSplitNode::Leaf(new_leaf.clone())),
                }
            },
        );
        if replaced {
            *self.inner.active_leaf_id.borrow_mut() = new_leaf.leaf_id;
            self.sync_split_dimming();
            self.trigger_rebuild(true);
        }
        replaced
    }

    fn close_leaf(&self, leaf_id: &str) -> bool {
        if self.inner.tree.borrow().leaf_count() <= 1 {
            return false;
        }
        let removed = self.inner.tree.borrow_mut().remove_leaf(leaf_id);
        if removed {
            let next_leaf_id = self.inner.tree.borrow().first_leaf().leaf_id.clone();
            *self.inner.active_leaf_id.borrow_mut() = next_leaf_id;
            self.sync_split_dimming();
            self.trigger_rebuild(true);
        }
        removed
    }

    fn trigger_rebuild(&self, focus_after_rebuild: bool) {
        self.cancel_swap();
        self.inner.swap_buttons.borrow_mut().clear();
        self.inner.active_resize_split.set(0);
        self.inner.resize_revealers.borrow_mut().clear();
        while let Some(child) = self.inner.resize_layer.first_child() {
            self.inner.resize_layer.remove(&child);
        }
        self.inner.focus_after_rebuild.set(focus_after_rebuild);
        if let Some(source) = self.inner.rebuild_source.borrow_mut().take() {
            source.remove();
        }
        self.inner.resize_overlay.set_child(gtk::Widget::NONE);
        self.schedule_rebuild();
    }

    fn schedule_rebuild(&self) {
        if self.inner.rebuild_source.borrow().is_some() {
            return;
        }
        let state = self.clone();
        let source = glib::idle_add_local_once(move || {
            state.inner.rebuild_source.replace(None);
            state.do_rebuild();
        });
        self.inner.rebuild_source.replace(Some(source));
    }

    fn do_rebuild(&self) {
        let tree = self.inner.tree.borrow();
        if terminal_tree_has_widget_parents(&tree) {
            detach_terminal_tree_widgets(&tree);
            drop(tree);
            self.schedule_rebuild();
            return;
        }
        self.inner
            .resize_overlay
            .set_child(Some(&build_terminal_split_widget_tree(&tree, self)));
        drop(tree);
        self.refresh_display();
        if self.inner.focus_after_rebuild.replace(false) {
            let handle = self.active_handle();
            glib::idle_add_local_once(move || {
                handle.focus_surface();
            });
        }
    }
}

impl Drop for TerminalTabInner {
    fn drop(&mut self) {
        if let Some(source) = self.rebuild_source.borrow_mut().take() {
            source.remove();
        }
    }
}

fn terminal_focus_index(
    current_index: usize,
    leaf_count: usize,
    direction: TerminalFocusDirection,
) -> Option<usize> {
    (leaf_count > 1).then(|| match direction {
        TerminalFocusDirection::Left => (current_index + leaf_count - 1) % leaf_count,
        TerminalFocusDirection::Right => (current_index + 1) % leaf_count,
    })
}

fn build_terminal_split_widget_tree(
    node: &TerminalSplitNode,
    state: &TerminalTabState,
) -> gtk::Widget {
    match node {
        TerminalSplitNode::Leaf(leaf) => {
            let overlay = gtk::Overlay::builder().hexpand(true).vexpand(true).build();
            overlay.set_child(Some(&leaf.widget));
            let button = gtk::Button::new();
            button.add_css_class("ghostdeck-swap-target");
            button.set_halign(gtk::Align::Fill);
            button.set_valign(gtk::Align::Fill);
            button.set_visible(false);
            let weak = Rc::downgrade(&state.inner);
            let leaf_id = leaf.leaf_id.clone();
            button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    TerminalTabState { inner }.finish_swap(&leaf_id);
                }
            });
            state
                .inner
                .swap_buttons
                .borrow_mut()
                .push((leaf.leaf_id.clone(), button.clone()));
            overlay.add_overlay(&button);
            overlay.upcast()
        }
        TerminalSplitNode::Split {
            orientation,
            ratio,
            start,
            end,
        } => {
            let split_orientation = *orientation;
            let paned = gtk::Paned::builder()
                .orientation(split_orientation)
                .hexpand(true)
                .vexpand(true)
                .build();
            paned.add_css_class(window::SPLIT_PANE_CSS_CLASS);
            paned.set_wide_handle(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);

            let resize_region = || {
                let guide = gtk::Box::new(split_orientation, 0);
                guide.add_css_class(if split_orientation == gtk::Orientation::Horizontal {
                    "ghostdeck-resize-guide-width"
                } else {
                    "ghostdeck-resize-guide-height"
                });
                if split_orientation == gtk::Orientation::Horizontal {
                    guide.set_halign(gtk::Align::Fill);
                    guide.set_valign(gtk::Align::Center);
                    guide.set_hexpand(true);
                    guide.set_margin_start(14);
                    guide.set_margin_end(14);
                } else {
                    guide.set_halign(gtk::Align::Center);
                    guide.set_valign(gtk::Align::Fill);
                    guide.set_vexpand(true);
                    guide.set_margin_top(14);
                    guide.set_margin_bottom(14);
                }
                let label = gtk::Label::new(None);
                label.add_css_class("ghostdeck-resize-share");
                label.set_halign(gtk::Align::Center);
                label.set_valign(gtk::Align::Center);
                label.set_can_target(false);
                let measurement = gtk::Overlay::builder().hexpand(true).vexpand(true).build();
                measurement.set_child(Some(&guide));
                measurement.add_overlay(&label);
                let revealer = gtk::Revealer::builder()
                    .transition_type(gtk::RevealerTransitionType::Crossfade)
                    .build();
                revealer.set_can_target(false);
                revealer.set_child(Some(&measurement));
                state.inner.resize_layer.put(&revealer, 0.0, 0.0);
                (label, revealer)
            };
            let (start_label, start_revealer) = resize_region();
            let (end_label, end_revealer) = resize_region();
            paned.set_start_child(Some(&build_terminal_split_widget_tree(start, state)));
            paned.set_end_child(Some(&build_terminal_split_widget_tree(end, state)));
            state
                .inner
                .resize_revealers
                .borrow_mut()
                .extend([start_revealer.clone(), end_revealer.clone()]);

            let split_id = paned.as_ptr() as usize;
            let click = gtk::GestureClick::new();
            click.set_button(1);
            click.set_propagation_phase(gtk::PropagationPhase::Capture);
            let paned_for_press = paned.clone();
            let resize_owner = Rc::downgrade(&state.inner);
            click.connect_pressed(move |_, _, x, y| {
                let position = paned_for_press.position() as f64;
                let coordinate = if split_orientation == gtk::Orientation::Horizontal {
                    x
                } else {
                    y
                };
                if (coordinate - position).abs() > 16.0 {
                    return;
                }
                if let Some(inner) = resize_owner.upgrade() {
                    inner.active_resize_split.set(split_id);
                    for revealer in inner.resize_revealers.borrow().iter() {
                        revealer.set_transition_duration(0);
                        revealer.set_reveal_child(false);
                    }
                }
            });
            paned.add_controller(click);

            // Ignore early position-notify churn until the first restored ratio
            // has actually been applied with a real allocation.
            let applying = Rc::new(Cell::new(true));
            let shared_ratio = ratio.clone();
            let orientation_for_notify = *orientation;
            let applying_for_notify = applying.clone();
            let tab_root = state.inner.root.downgrade();
            let resize_layer = state.inner.resize_layer.downgrade();
            let resize_owner = Rc::downgrade(&state.inner);
            let hide_source = Rc::new(RefCell::new(None::<glib::SourceId>));
            paned.connect_position_notify(move |paned| {
                if applying_for_notify.get() {
                    return;
                }
                let Some(inner) = resize_owner.upgrade() else {
                    return;
                };
                let active_split = inner.active_resize_split.get();
                if active_split != 0 && active_split != split_id {
                    return;
                }
                let Some(tab_root) = tab_root.upgrade() else {
                    return;
                };
                let Some(resize_layer) = resize_layer.upgrade() else {
                    return;
                };
                let allocation = paned.allocation();
                let size = if orientation_for_notify == gtk::Orientation::Horizontal {
                    allocation.width()
                } else {
                    allocation.height()
                };
                let stored_ratio = *shared_ratio.borrow();
                *shared_ratio.borrow_mut() =
                    layout_state::snapshot_split_ratio(paned.position(), size, Some(stored_ratio));

                if active_split != split_id {
                    return;
                }

                let (total, glyph, axis) = if orientation_for_notify == gtk::Orientation::Horizontal
                {
                    (tab_root.allocated_width(), "↔", "width")
                } else {
                    (tab_root.allocated_height(), "↕", "height")
                };
                if total <= 0 {
                    return;
                }
                let Some(bounds) = paned.compute_bounds(&tab_root) else {
                    return;
                };
                let (x, y, width, height) = (
                    bounds.x() as i32,
                    bounds.y() as i32,
                    bounds.width() as i32,
                    bounds.height() as i32,
                );
                let divider = if orientation_for_notify == gtk::Orientation::Horizontal {
                    (x + paned.position()).clamp(0, total)
                } else {
                    (y + paned.position()).clamp(0, total)
                };
                let (start_rect, end_rect) =
                    if orientation_for_notify == gtk::Orientation::Horizontal {
                        (
                            (0, y, divider, height),
                            (divider, y, total - divider, height),
                        )
                    } else {
                        ((x, 0, width, divider), (x, divider, width, total - divider))
                    };
                for (revealer, (x, y, width, height)) in
                    [(&start_revealer, start_rect), (&end_revealer, end_rect)]
                {
                    resize_layer.move_(revealer, x as f64, y as f64);
                    revealer.set_size_request(width.max(1), height.max(1));
                }
                start_label.set_label(&format!(
                    "{glyph} {}% {axis}",
                    (divider as f64 * 100.0 / total as f64).round()
                ));
                end_label.set_label(&format!(
                    "{glyph} {}% {axis}",
                    100.0 - (divider as f64 * 100.0 / total as f64).round()
                ));
                start_revealer.set_transition_duration(0);
                end_revealer.set_transition_duration(0);
                start_revealer.set_reveal_child(true);
                end_revealer.set_reveal_child(true);

                if let Some(source) = hide_source.borrow_mut().take() {
                    source.remove();
                }
                let start_revealer_for_hide = start_revealer.clone();
                let end_revealer_for_hide = end_revealer.clone();
                let hide_source_for_timeout = hide_source.clone();
                let source = glib::timeout_add_local_once(
                    std::time::Duration::from_millis(1_500),
                    move || {
                        start_revealer_for_hide.set_transition_duration(700);
                        end_revealer_for_hide.set_transition_duration(700);
                        start_revealer_for_hide.set_reveal_child(false);
                        end_revealer_for_hide.set_reveal_child(false);
                        hide_source_for_timeout.borrow_mut().take();
                    },
                );
                hide_source.borrow_mut().replace(source);
            });

            window::apply_split_ratio_after_layout(
                &paned,
                split_orientation,
                ratio.clone(),
                applying,
            );
            paned.upcast()
        }
    }
}

fn terminal_tree_has_widget_parents(node: &TerminalSplitNode) -> bool {
    match node {
        TerminalSplitNode::Leaf(leaf) => leaf.widget.parent().is_some(),
        TerminalSplitNode::Split { start, end, .. } => {
            terminal_tree_has_widget_parents(start) || terminal_tree_has_widget_parents(end)
        }
    }
}

fn detach_terminal_tree_widgets(node: &TerminalSplitNode) {
    match node {
        TerminalSplitNode::Leaf(leaf) => {
            if let Some(parent) = leaf.widget.parent() {
                if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
                    if paned
                        .start_child()
                        .map(|child| child == leaf.widget)
                        .unwrap_or(false)
                    {
                        paned.set_start_child(gtk::Widget::NONE);
                    } else {
                        paned.set_end_child(gtk::Widget::NONE);
                    }
                } else if let Some(container) = parent.downcast_ref::<gtk::Box>() {
                    container.remove(&leaf.widget);
                } else if let Some(overlay) = parent.downcast_ref::<gtk::Overlay>() {
                    if overlay.child().as_ref() == Some(&leaf.widget) {
                        overlay.set_child(gtk::Widget::NONE);
                    }
                }
            }
        }
        TerminalSplitNode::Split { start, end, .. } => {
            detach_terminal_tree_widgets(start);
            detach_terminal_tree_widgets(end);
        }
    }
}

#[derive(Clone)]
pub struct TerminalShortcutTarget {
    handle: terminal::TerminalHandle,
}

impl TerminalShortcutTarget {
    pub fn perform_binding_action(&self, action: &str) -> bool {
        self.handle.perform_binding_action(action)
    }

    pub fn show_find(&self) -> bool {
        self.handle.show_find()
    }

    pub fn find_next(&self) -> bool {
        self.handle.find_next()
    }

    pub fn find_previous(&self) -> bool {
        self.handle.find_previous()
    }

    pub fn hide_find(&self) -> bool {
        self.handle.hide_find()
    }

    pub fn use_selection_for_find(&self) -> bool {
        self.handle.use_selection_for_find()
    }
}

#[derive(Clone)]
pub enum FocusedShortcutTarget {
    None,
    Terminal(TerminalShortcutTarget),
    Keybinds,
}

#[derive(Clone)]
struct TabContextMenuContext {
    tab_strip: gtk::Box,
    content_stack: gtk::Stack,
    tab_state: Rc<RefCell<TabState>>,
    callbacks: Rc<PaneCallbacks>,
    pane_outer: gtk::Box,
    label: gtk::Label,
    pin_icon: gtk::Label,
}

// ---------------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------------

pub const PANE_CSS: &str = r#"
.ghostdeck-pane-header {
    background-color: @window_bg_color;
    color: @window_fg_color;
    border-bottom: 1px solid alpha(@window_fg_color, 0.08);
    min-height: 30px;
    padding: 0 2px;
}
.ghostdeck-tab {
    background: none;
    border: none;
    border-radius: 4px 4px 0 0;
    padding: 4px 4px 4px 10px;
    color: alpha(@window_fg_color, 0.5);
    min-height: 0;
    font-size: 12px;
}
.ghostdeck-tab:hover {
    color: alpha(@window_fg_color, 0.72);
    background: alpha(@window_fg_color, 0.04);
}
.ghostdeck-tab-active {
    color: @window_fg_color;
    background: alpha(@window_fg_color, 0.08);
}
.ghostdeck-tab-close {
    background: none;
    border: none;
    border-radius: 3px;
    padding: 1px;
    min-height: 0;
    min-width: 0;
    color: alpha(@window_fg_color, 0.28);
    margin-left: 4px;
}
.ghostdeck-tab-close:hover {
    color: alpha(@window_fg_color, 0.8);
    background: alpha(@window_fg_color, 0.1);
}
.ghostdeck-pane-action {
    background: none;
    border: none;
    border-radius: 4px;
    padding: 4px 5px;
    min-height: 0;
    min-width: 0;
    color: alpha(@window_fg_color, 0.4);
}
.ghostdeck-pane-action:hover {
    background: alpha(@window_fg_color, 0.08);
    color: alpha(@window_fg_color, 0.8);
}
.ghostdeck-resize-share {
    background: alpha(@window_bg_color, 0.9);
    color: @window_fg_color;
    border: 1px solid alpha(@window_fg_color, 0.2);
    border-radius: 999px;
    padding: 6px 10px;
    font-size: 16px;
    font-weight: 700;
}
.ghostdeck-resize-guide-width {
    min-height: 7px;
    border: 1px solid @accent_bg_color;
    border-bottom: none;
}
.ghostdeck-resize-guide-height {
    min-width: 7px;
    border: 1px solid @accent_bg_color;
    border-right: none;
}
.ghostdeck-swap-target {
    background: alpha(@window_bg_color, 0.7);
    color: @window_fg_color;
    border: 2px solid @accent_color;
    border-radius: 0;
    font-size: 32px;
    font-weight: 700;
}
.ghostdeck-swap-target:hover {
    background: alpha(@accent_bg_color, 0.35);
}
.ghostdeck-swap-target.ghostdeck-swap-source {
    background: alpha(@window_bg_color, 0.45);
    border-color: alpha(@window_fg_color, 0.4);
}
.ghostdeck-split-icon {
    border: 1px solid alpha(@window_fg_color, 0.4);
    border-radius: 2px;
    min-width: 16px;
    min-height: 12px;
    padding: 0;
}
.ghostdeck-split-icon:hover {
    border-color: alpha(@window_fg_color, 0.8);
}
.ghostdeck-split-half-v {
    min-width: 6px;
    min-height: 10px;
}
.ghostdeck-split-half-h {
    min-width: 14px;
    min-height: 4px;
}
.ghostdeck-split-btn {
    background: none;
    border: none;
    border-radius: 4px;
    padding: 4px 5px;
    min-height: 0;
    min-width: 0;
}
.ghostdeck-split-btn:hover {
    background: alpha(@window_fg_color, 0.08);
}
.ghostdeck-pin-icon {
    font-size: 9px;
    margin-right: 2px;
}
.ghostdeck-tab-rename-entry {
    padding: 1px 4px;
    min-height: 0;
    font-size: 12px;
}
.ghostdeck-tab-drop-indicator {
    background-color: @accent_bg_color;
    min-width: 2px;
    margin: 2px 0;
}
.ghostdeck-tab-overlay:drop(active) {
    box-shadow: none;
}
"#;

// ---------------------------------------------------------------------------
// PaneWidget builder
// ---------------------------------------------------------------------------

pub fn create_pane(
    callbacks: Rc<PaneCallbacks>,
    shortcuts: Rc<ResolvedShortcutConfig>,
    working_directory: Option<&str>,
    initial_state: Option<&PaneState>,
    skip_default_tab: bool,
) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    outer.set_size_request(MIN_PANE_WIDTH, MIN_PANE_HEIGHT);

    // The single header line: tabs (left) + action icons (right)
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .build();
    header.add_css_class("ghostdeck-pane-header");

    let tab_overlay = gtk::Overlay::new();
    tab_overlay.add_css_class("ghostdeck-tab-overlay");
    tab_overlay.set_hexpand(true);

    let tab_strip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .hexpand(true)
        .build();
    tab_overlay.set_child(Some(&tab_strip));

    let drop_indicator = gtk::Box::new(gtk::Orientation::Vertical, 0);
    drop_indicator.add_css_class("ghostdeck-tab-drop-indicator");
    drop_indicator.set_halign(gtk::Align::Start);
    drop_indicator.set_valign(gtk::Align::Fill);
    drop_indicator.set_visible(false);
    tab_overlay.add_overlay(&drop_indicator);
    tab_overlay.set_clip_overlay(&drop_indicator, false);

    let content_stack = gtk::Stack::new();
    content_stack.set_transition_type(gtk::StackTransitionType::None);
    content_stack.set_hexpand(true);
    content_stack.set_vexpand(true);

    // Action icons (right side)
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(1)
        .build();

    let new_term_btn = icon_button(
        "utilities-terminal-symbolic",
        &pane_action_tooltip(
            &shortcuts,
            "New terminal tab",
            Some(ShortcutId::NewTerminal),
        ),
    );
    let split_h_btn = icon_button(
        "ghostdeck-split-horizontal-symbolic",
        &pane_action_tooltip(&shortcuts, "Split right", Some(ShortcutId::SplitRight)),
    );
    let split_v_btn = icon_button(
        "ghostdeck-split-vertical-symbolic",
        &pane_action_tooltip(&shortcuts, "Split down", Some(ShortcutId::SplitDown)),
    );
    let settings_btn = icon_button("emblem-system-symbolic", "Settings");

    actions.append(&new_term_btn);
    actions.append(&split_h_btn);
    actions.append(&split_v_btn);
    actions.append(&settings_btn);

    header.append(&tab_overlay);
    header.append(&actions);

    outer.append(&header);
    outer.append(&content_stack);

    let ws_wd = Rc::new(RefCell::new(
        working_directory.map(|value| value.to_string()),
    ));
    let tab_state = Rc::new(RefCell::new(TabState {
        tabs: Vec::new(),
        active_tab: None,
    }));
    let workspace_dragging = Rc::new(Cell::new(false));
    let pane_id = pane_id_for_initial_state(initial_state);
    let internals = Rc::new(PaneInternals {
        pane_id,
        tab_state: tab_state.clone(),
        tab_strip: tab_strip.clone(),
        content_stack: content_stack.clone(),
        drop_indicator: drop_indicator.clone(),
        pane_outer: outer.clone(),
        callbacks: callbacks.clone(),
        working_directory: ws_wd.clone(),
        workspace_dragging: workspace_dragging.clone(),
        new_terminal_button: new_term_btn.clone(),
        split_right_button: split_h_btn.clone(),
        split_down_button: split_v_btn.clone(),
    });

    if let Some(saved_state) = initial_state {
        restore_tabs_from_state(&internals, working_directory, saved_state);
    } else if !skip_default_tab {
        add_terminal_tab_inner(&internals, working_directory, None);
    }

    {
        let internals = internals.clone();
        let wd = ws_wd.clone();
        new_term_btn.connect_clicked(move |_| {
            let dir = wd.borrow().clone();
            add_terminal_tab_inner(&internals, dir.as_deref(), None);
        });
    }
    {
        let pw = outer.clone();
        split_h_btn.connect_clicked(move |_| {
            let pane_widget: gtk::Widget = pw.clone().upcast();
            split_active_terminal_tab_in_pane(&pane_widget, gtk::Orientation::Horizontal);
        });
    }
    {
        let pw = outer.clone();
        split_v_btn.connect_clicked(move |_| {
            let pane_widget: gtk::Widget = pw.clone().upcast();
            split_active_terminal_tab_in_pane(&pane_widget, gtk::Orientation::Vertical);
        });
    }
    {
        let internals = internals.clone();
        settings_btn.connect_clicked(move |_| {
            settings_editor::present_settings_dialog(
                &internals.pane_outer,
                settings_editor::SettingsEditorInput {
                    config: (internals.callbacks.current_config)(),
                    shortcuts: (internals.callbacks.current_shortcuts)(),
                    on_capture: internals.callbacks.on_capture_shortcut.clone(),
                    on_config_changed: internals.callbacks.on_config_changed.clone(),
                },
            );
        });
    }

    install_tab_strip_drop_target(&tab_overlay, &internals);

    register_pane(pane_id, &internals);
    unsafe {
        outer.set_data("ghostdeck-pane-internals", internals);
    }
    outer.connect_destroy(move |_| {
        unregister_pane(pane_id);
    });

    outer
}

/// Cycle tabs in the focused pane. `delta`: 1 = next, -1 = prev.
pub fn cycle_tab_in_pane(pane_widget: &gtk::Widget, delta: i32) {
    let outer = pane_widget.downcast_ref::<gtk::Box>();
    let outer = match outer {
        Some(o) => o,
        None => return,
    };
    let internals: Rc<PaneInternals> = unsafe {
        match outer.data::<Rc<PaneInternals>>("ghostdeck-pane-internals") {
            Some(ptr) => ptr.as_ref().clone(),
            None => return,
        }
    };

    let ts = internals.tab_state.borrow();
    let len = ts.tabs.len();
    if len <= 1 {
        return;
    }

    let active_idx = ts
        .active_tab
        .as_ref()
        .and_then(|id| ts.tabs.iter().position(|e| e.id == *id))
        .unwrap_or(0);

    let new_idx = (active_idx as i32 + delta).rem_euclid(len as i32) as usize;
    let new_id = ts.tabs[new_idx].id.clone();
    drop(ts);

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &new_id,
    );
    (internals.callbacks.on_state_changed)();
}

pub fn focus_active_tab_in_pane(pane_widget: &gtk::Widget) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };

    let target_tab_id = {
        let tab_state = internals.tab_state.borrow();
        tab_state
            .active_tab
            .clone()
            .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()))
    };

    let Some(tab_id) = target_tab_id else {
        return false;
    };

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &tab_id,
    );
    true
}

pub fn activate_tab_in_pane(pane_widget: &gtk::Widget, tab_id: &str) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };

    let has_tab = internals
        .tab_state
        .borrow()
        .tabs
        .iter()
        .any(|entry| entry.id == tab_id);
    if !has_tab {
        return false;
    }

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        tab_id,
    );
    true
}

fn normalize_surface_hint(raw: &str) -> &str {
    raw.trim()
        .strip_prefix("surface:")
        .unwrap_or_else(|| raw.trim())
}

fn composite_surface_id(pane_id: u32, tab_id: &str) -> String {
    format!("{pane_id}:{tab_id}")
}

fn surface_hint_matches(
    surface_id: &str,
    tab_surface_id: &str,
    tab_id: &str,
    surface_hint: &str,
) -> bool {
    let requested = normalize_surface_hint(surface_hint);
    !requested.is_empty()
        && (requested == tab_id || requested == tab_surface_id || requested == surface_id)
}

pub fn terminal_handle_for_surface(
    pane_widget: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Option<(String, terminal::TerminalHandle)> {
    let internals = find_pane_internals(pane_widget)?;
    let pane_id = internals.pane_id;
    let tab_state = internals.tab_state.borrow();
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());
    let active_tab = tab_state.active_tab.as_deref();
    let mut fallback = None;

    for entry in &tab_state.tabs {
        let TabKind::Terminal { state } = &entry.kind else {
            continue;
        };
        let tab_surface_id = composite_surface_id(pane_id, &entry.id);
        let active_leaf_id = state.active_leaf_id();
        let mut matched = None;
        state.inner.tree.borrow().for_each_leaf(|leaf| {
            let surface_id = terminal_surface_id(pane_id, &entry.id, &leaf.leaf_id);
            if requested.is_some_and(|value| {
                value == surface_id || value == tab_surface_id || value == entry.id
            }) {
                matched = Some((surface_id, leaf.handle.clone()));
                return;
            }
            if active_tab == Some(entry.id.as_str()) && leaf.leaf_id == active_leaf_id {
                matched = Some((surface_id, leaf.handle.clone()));
            } else if fallback.is_none() {
                fallback = Some((surface_id, leaf.handle.clone()));
            }
        });
        if let Some(matched) = matched {
            return Some(matched);
        }
    }

    fallback
}

pub fn exact_terminal_handle_for_surface(
    pane_widget: &gtk::Widget,
    surface_hint: &str,
) -> Option<(String, terminal::TerminalHandle)> {
    let internals = find_pane_internals(pane_widget)?;
    let pane_id = internals.pane_id;
    let tab_state = internals.tab_state.borrow();

    for entry in &tab_state.tabs {
        let TabKind::Terminal { state } = &entry.kind else {
            continue;
        };
        let tab_surface_id = composite_surface_id(pane_id, &entry.id);
        let mut matched = None;
        state.inner.tree.borrow().for_each_leaf(|leaf| {
            let surface_id = terminal_surface_id(pane_id, &entry.id, &leaf.leaf_id);
            if surface_hint_matches(&surface_id, &tab_surface_id, &entry.id, surface_hint) {
                matched = Some((surface_id, leaf.handle.clone()));
            }
        });
        if let Some(matched) = matched {
            return Some(matched);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Internal tab state
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum TabKind {
    Terminal { state: TerminalTabState },
    Keybinds,
}

enum TabFocusTarget {
    Terminal(terminal::TerminalHandle),
    Widget(gtk::Widget),
}

impl TabFocusTarget {
    fn from_entry(entry: &TabEntry) -> Self {
        match &entry.kind {
            TabKind::Terminal { state } => Self::Terminal(state.active_handle()),
            TabKind::Keybinds => Self::Widget(entry.content.clone()),
        }
    }

    fn focus(self) {
        match self {
            Self::Terminal(handle) => {
                handle.focus_surface();
            }
            Self::Widget(widget) => {
                if widget.is_focus() || widget.can_focus() {
                    widget.grab_focus();
                } else {
                    widget.child_focus(gtk::DirectionType::TabForward);
                }
            }
        }
    }
}

struct TabEntry {
    id: String,
    tab_button: gtk::Box,
    title_label: gtk::Label,
    content: gtk::Widget,
    custom_name: Option<String>,
    pinned: bool,
    kind: TabKind,
}

struct TabState {
    tabs: Vec<TabEntry>,
    active_tab: Option<String>,
}

/// Shared internals stored on the pane outer Box for external access.
pub struct PaneInternals {
    pane_id: u32,
    tab_state: Rc<std::cell::RefCell<TabState>>,
    tab_strip: gtk::Box,
    content_stack: gtk::Stack,
    drop_indicator: gtk::Box,
    pane_outer: gtk::Box,
    callbacks: Rc<PaneCallbacks>,
    working_directory: Rc<std::cell::RefCell<Option<String>>>,
    workspace_dragging: Rc<Cell<bool>>,
    new_terminal_button: gtk::Button,
    split_right_button: gtk::Button,
    split_down_button: gtk::Button,
}

impl TabState {
    fn find_tab_mut(&mut self, id: &str) -> Option<&mut TabEntry> {
        self.tabs.iter_mut().find(|e| e.id == id)
    }
}

fn next_tab_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Icon button helper
// ---------------------------------------------------------------------------

fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let btn = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(tooltip)
        .has_frame(false)
        .build();
    btn.add_css_class("ghostdeck-pane-action");
    btn
}

fn pane_action_tooltip(
    shortcuts: &ResolvedShortcutConfig,
    base: &str,
    shortcut_id: Option<ShortcutId>,
) -> String {
    shortcut_id
        .map(|id| shortcuts.tooltip_text(id, base))
        .unwrap_or_else(|| base.to_string())
}

/// Create a split-pane icon button with two rectangles separated by a divider.
/// Horizontal = left|right panes, Vertical = top/bottom panes.
#[allow(dead_code)]
fn split_icon_button(orientation: gtk::Orientation, tooltip: &str) -> gtk::Button {
    let icon = gtk::Box::new(orientation, 1);
    icon.add_css_class("ghostdeck-split-icon");

    let (class_name, count) = match orientation {
        gtk::Orientation::Horizontal => ("ghostdeck-split-half-v", 2),
        _ => ("ghostdeck-split-half-h", 2),
    };

    for _ in 0..count {
        let half = gtk::Box::new(gtk::Orientation::Vertical, 0);
        half.add_css_class(class_name);
        icon.append(&half);
    }

    let btn = gtk::Button::builder()
        .child(&icon)
        .tooltip_text(tooltip)
        .has_frame(false)
        .build();
    btn.add_css_class("ghostdeck-split-btn");
    btn
}

// ---------------------------------------------------------------------------
// Tab creation
// ---------------------------------------------------------------------------

struct TerminalTabOptions<'a> {
    id: Option<&'a str>,
    custom_name: Option<&'a str>,
    pinned: bool,
    cwd: Option<&'a str>,
    agent: Option<RestorableAgentState>,
    tree: Option<&'a layout_state::TerminalTreeState>,
    active_leaf_id: Option<&'a str>,
}

struct KeybindsTabOptions<'a> {
    id: Option<&'a str>,
    custom_name: Option<&'a str>,
    pinned: bool,
}

struct KeybindsTabInput<'a> {
    shortcuts: Rc<ResolvedShortcutConfig>,
    on_capture: Rc<PaneShortcutCaptureCallback>,
    options: Option<KeybindsTabOptions<'a>>,
}

fn restore_tabs_from_state(
    internals: &Rc<PaneInternals>,
    working_directory: Option<&str>,
    saved_state: &PaneState,
) {
    if saved_state.tabs.is_empty() {
        add_terminal_tab_inner(internals, working_directory, None);
        return;
    }

    for saved_tab in &saved_state.tabs {
        match &saved_tab.content {
            TabContentState::Terminal {
                cwd,
                agent,
                tree,
                active_leaf_id,
            } => add_terminal_tab_inner(
                internals,
                cwd.as_deref().or(working_directory),
                Some(TerminalTabOptions {
                    id: Some(saved_tab.id.as_str()),
                    custom_name: saved_tab.custom_name.as_deref(),
                    pinned: saved_tab.pinned,
                    cwd: cwd.as_deref().or(working_directory),
                    agent: agent.clone(),
                    tree: tree.as_deref(),
                    active_leaf_id: active_leaf_id.as_deref(),
                }),
            ),
            TabContentState::Browser { .. } => {}
            TabContentState::Keybinds {} => add_keybind_editor_tab_inner(
                internals,
                KeybindsTabInput {
                    shortcuts: (internals.callbacks.current_shortcuts)(),
                    on_capture: internals.callbacks.on_capture_shortcut.clone(),
                    options: Some(KeybindsTabOptions {
                        id: Some(saved_tab.id.as_str()),
                        custom_name: saved_tab.custom_name.as_deref(),
                        pinned: saved_tab.pinned,
                    }),
                },
            ),
            // Settings now open in a transient dialog rather than a persisted tab.
            TabContentState::Settings {} => {}
        }
    }

    if internals.tab_state.borrow().tabs.is_empty() {
        add_terminal_tab_inner(internals, working_directory, None);
    }

    let active_tab_id = saved_state
        .active_tab_id
        .as_deref()
        .filter(|candidate| {
            internals
                .tab_state
                .borrow()
                .tabs
                .iter()
                .any(|tab| tab.id == *candidate)
        })
        .map(|value| value.to_string())
        .or_else(|| {
            internals
                .tab_state
                .borrow()
                .tabs
                .first()
                .map(|tab| tab.id.clone())
        });

    if let Some(active_tab_id) = active_tab_id {
        activate_tab(
            &internals.tab_strip,
            &internals.content_stack,
            &internals.tab_state,
            &active_tab_id,
        );
    }
}

fn next_leaf_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn terminal_surface_id(pane_id: u32, tab_id: &str, leaf_id: &str) -> String {
    format!("{pane_id}:{tab_id}:{leaf_id}")
}

fn resolve_terminal_working_directory<'a>(
    cwd: Option<&'a str>,
    fallback: Option<&'a str>,
) -> Option<&'a str> {
    cwd.or(fallback)
}

fn placeholder_terminal_callbacks() -> TerminalCallbacks {
    TerminalCallbacks {
        on_title_changed: Box::new(|_| {}),
        on_pwd_changed: Box::new(|_| {}),
        on_desktop_notification: Box::new(|_, _, _| {}),
        on_bell: Box::new(|_| {}),
        on_focus: Box::new(|| {}),
        on_close: Box::new(|| {}),
        on_open_url: Box::new(|_, _| {}),
        on_split_right: Box::new(|| {}),
        on_split_down: Box::new(|| {}),
        on_swap: Box::new(|| {}),
    }
}

fn create_terminal_leaf(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    leaf_id: &str,
    working_directory: Option<&str>,
    cwd: Option<&str>,
    agent: Option<RestorableAgentState>,
) -> TerminalLeafState {
    let working_directory = resolve_terminal_working_directory(cwd, working_directory);
    let surface_id = terminal_surface_id(internals.pane_id, tab_id, leaf_id);
    let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
    let mut extra_env = vec![
        ("GHOSTDECK_SURFACE_ID".to_string(), surface_id.clone()),
        ("GHOSTDECK_PANE_ID".to_string(), internals.pane_id.to_string()),
        ("GHOSTDECK_TAB_ID".to_string(), tab_id.to_string()),
    ];
    if let Some(workspace_id) = (internals.callbacks.workspace_for_pane)(&pane_widget) {
        extra_env.push(("GHOSTDECK_WORKSPACE_ID".to_string(), workspace_id));
    }
    if let Some(socket) = ghostdeck_control::socket_path::resolve_socket_path(
        None,
        ghostdeck_control::socket_path::SocketMode::Runtime,
    )
    .to_str()
    {
        extra_env.push(("GHOSTDECK_SOCKET".to_string(), socket.to_string()));
    }
    let term_cwd = Rc::new(RefCell::new(working_directory.map(str::to_string)));
    let term_agent = Rc::new(RefCell::new(agent.clone()));
    let hover_focus = {
        let callbacks = internals.callbacks.clone();
        Rc::new(move || {
            let config = (callbacks.current_config)();
            let hover_focus = config.borrow().focus.hover_terminal_focus;
            hover_focus
        })
    };
    let startup_command = agent.as_ref().and_then(|agent| agent.resume_command());
    if let Some(command) = startup_command.as_deref() {
        eprintln!(
            "ghostdeck: restoring agent terminal surface={} command={}",
            terminal_surface_id(internals.pane_id, tab_id, leaf_id),
            command
        );
    }
    let term = terminal::create_terminal(
        working_directory,
        terminal::TerminalOptions {
            hover_focus,
            saved_font_size: (internals.callbacks.current_config)().borrow().font_size,
            startup_command,
            extra_env,
        },
        placeholder_terminal_callbacks(),
    );
    TerminalLeafState {
        leaf_id: leaf_id.to_string(),
        surface_id,
        creator_surface_id: None,
        cwd: term_cwd,
        agent: term_agent,
        handle: term.handle,
        widget: term.root,
    }
}

fn runtime_terminal_tree_from_layout(
    internals: &Rc<PaneInternals>,
    tab_id: &str,
    working_directory: Option<&str>,
    tree: &layout_state::TerminalTreeState,
) -> TerminalSplitNode {
    match tree {
        layout_state::TerminalTreeState::Leaf(leaf) => {
            let mut runtime_leaf = create_terminal_leaf(
                internals,
                tab_id,
                leaf.leaf_id.as_deref().unwrap_or("leaf-0"),
                working_directory,
                leaf.cwd.as_deref(),
                leaf.agent.clone(),
            );
            runtime_leaf.creator_surface_id = leaf.creator_surface_id.clone();
            TerminalSplitNode::Leaf(runtime_leaf)
        }
        layout_state::TerminalTreeState::Split(split) => TerminalSplitNode::Split {
            orientation: if split.orientation == layout_state::SplitOrientation::Horizontal {
                gtk::Orientation::Horizontal
            } else {
                gtk::Orientation::Vertical
            },
            ratio: Rc::new(RefCell::new(layout_state::clamp_split_ratio(split.ratio))),
            start: Box::new(runtime_terminal_tree_from_layout(
                internals,
                tab_id,
                working_directory,
                &split.start,
            )),
            end: Box::new(runtime_terminal_tree_from_layout(
                internals,
                tab_id,
                working_directory,
                &split.end,
            )),
        },
    }
}

fn split_terminal_tab_leaf(
    internals: &Rc<PaneInternals>,
    terminal_tab_state: &TerminalTabState,
    tab_id: &str,
    title_label: &gtk::Label,
    source_leaf: &TerminalLeafState,
    orientation: gtk::Orientation,
) -> bool {
    let new_leaf = create_terminal_leaf(
        internals,
        tab_id,
        &next_leaf_id(),
        source_leaf.cwd.borrow().as_deref(),
        source_leaf.cwd.borrow().as_deref(),
        None,
    );
    if terminal_tab_state.split_leaf(&source_leaf.leaf_id, new_leaf, orientation, false) {
        let state = terminal_tab_state.clone();
        terminal_tab_state.replace_callbacks(|leaf| {
            make_terminal_callbacks(internals, &state, tab_id, title_label, leaf)
        });
        (internals.callbacks.on_state_changed)();
        return true;
    }
    false
}

pub fn split_active_terminal_tab_in_pane(
    pane_widget: &gtk::Widget,
    orientation: gtk::Orientation,
) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };
    let (terminal_tab_state, tab_id, title_label) = {
        let tab_state = internals.tab_state.borrow();
        let active_id = tab_state
            .active_tab
            .clone()
            .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()));
        let Some(active_id) = active_id else {
            return false;
        };
        let Some(entry) = tab_state.tabs.iter().find(|entry| entry.id == active_id) else {
            return false;
        };
        let TabKind::Terminal { state } = &entry.kind else {
            return false;
        };
        (state.clone(), entry.id.clone(), entry.title_label.clone())
    };
    let source_leaf = terminal_tab_state.active_leaf();
    split_terminal_tab_leaf(
        &internals,
        &terminal_tab_state,
        &tab_id,
        &title_label,
        &source_leaf,
        orientation,
    )
}

pub fn start_swap_in_active_terminal_tab(pane_widget: &gtk::Widget) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };
    let state = {
        let tab_state = internals.tab_state.borrow();
        let Some(active_id) = tab_state.active_tab.as_deref() else {
            return false;
        };
        let Some(entry) = tab_state.tabs.iter().find(|entry| entry.id == active_id) else {
            return false;
        };
        let TabKind::Terminal { state } = &entry.kind else {
            return false;
        };
        state.clone()
    };
    if state.leaf_count() < 2 {
        return false;
    }
    let source = state.active_leaf_id();
    state.start_swap(&source);
    true
}

fn terminal_tab_context(
    pane_widget: &gtk::Widget,
    tab_id: &str,
) -> Result<(Rc<PaneInternals>, TerminalTabState, gtk::Label), TerminalTabSurfaceError> {
    let internals =
        find_pane_internals(pane_widget).ok_or(TerminalTabSurfaceError::PaneNotFound)?;
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state
        .tabs
        .iter()
        .find(|entry| entry.id == tab_id)
        .ok_or(TerminalTabSurfaceError::TabNotFound)?;
    let TabKind::Terminal { state } = &entry.kind else {
        return Err(TerminalTabSurfaceError::NotTerminal);
    };
    let context = (internals.clone(), state.clone(), entry.title_label.clone());
    drop(tab_state);
    Ok(context)
}

pub fn add_surface_to_terminal_tab(
    pane_widget: &gtk::Widget,
    tab_id: &str,
    source_surface_id: &str,
    cwd: Option<&str>,
) -> Result<SurfaceSummary, TerminalTabSurfaceError> {
    let (internals, terminal_tab_state, title_label) = terminal_tab_context(pane_widget, tab_id)?;
    let source_leaf = {
        let tree = terminal_tab_state.inner.tree.borrow();
        tree.find_surface(internals.pane_id, tab_id, source_surface_id)
            .cloned()
            .ok_or(TerminalTabSurfaceError::SourceNotFound)?
    };
    let leaf_count = terminal_tab_state.leaf_count();
    if leaf_count >= 4 {
        return Err(TerminalTabSurfaceError::LimitReached);
    }

    {
        let tree = terminal_tab_state.inner.tree.borrow();
        let valid = match (&*tree, leaf_count) {
            (TerminalSplitNode::Leaf(_), 1) => true,
            (
                TerminalSplitNode::Split {
                    orientation,
                    start,
                    end,
                    ..
                },
                2,
            ) => {
                *orientation == gtk::Orientation::Horizontal
                    && matches!(start.as_ref(), TerminalSplitNode::Leaf(_))
                    && matches!(end.as_ref(), TerminalSplitNode::Leaf(_))
            }
            (
                TerminalSplitNode::Split {
                    orientation,
                    start,
                    end,
                    ..
                },
                3,
            ) => {
                *orientation == gtk::Orientation::Horizontal
                    && matches!(start.as_ref(), TerminalSplitNode::Leaf(_))
                    && matches!(end.as_ref(), TerminalSplitNode::Split { orientation, start, end, .. }
                        if *orientation == gtk::Orientation::Vertical
                            && matches!(start.as_ref(), TerminalSplitNode::Leaf(_))
                            && matches!(end.as_ref(), TerminalSplitNode::Leaf(_)))
            }
            _ => false,
        };
        if !valid {
            return Err(TerminalTabSurfaceError::UnsupportedLayout);
        }
    }

    let surface_cwd = cwd
        .map(str::to_string)
        .or_else(|| source_leaf.cwd.borrow().clone());
    let mut new_leaf = create_terminal_leaf(
        &internals,
        tab_id,
        &next_leaf_id(),
        surface_cwd.as_deref(),
        surface_cwd.as_deref(),
        None,
    );
    new_leaf.creator_surface_id = Some(source_leaf.surface_id.clone());
    let new_surface_id = terminal_surface_id(internals.pane_id, tab_id, &new_leaf.leaf_id);
    {
        let mut tree = terminal_tab_state.inner.tree.borrow_mut();
        if leaf_count == 1 {
            *tree = TerminalSplitNode::Split {
                orientation: gtk::Orientation::Horizontal,
                ratio: Rc::new(RefCell::new(layout_state::DEFAULT_SPLIT_RATIO)),
                start: Box::new(tree.clone()),
                end: Box::new(TerminalSplitNode::Leaf(new_leaf.clone())),
            };
        } else if let TerminalSplitNode::Split { end, .. } = &mut *tree {
            let mut column_leaves = Vec::with_capacity(leaf_count);
            end.for_each_leaf(|leaf| column_leaves.push(leaf.clone()));
            column_leaves.push(new_leaf.clone());
            let mut column = TerminalSplitNode::Leaf(column_leaves.remove(0));
            for (index, leaf) in column_leaves.into_iter().enumerate() {
                let placed = index + 1;
                column = TerminalSplitNode::Split {
                    orientation: gtk::Orientation::Vertical,
                    ratio: Rc::new(RefCell::new(placed as f64 / (placed + 1) as f64)),
                    start: Box::new(column),
                    end: Box::new(TerminalSplitNode::Leaf(leaf)),
                };
            }
            **end = column;
        }
    }

    *terminal_tab_state.inner.active_leaf_id.borrow_mut() = source_leaf.leaf_id;
    terminal_tab_state.replace_callbacks(|leaf| {
        make_terminal_callbacks(&internals, &terminal_tab_state, tab_id, &title_label, leaf)
    });
    terminal_tab_state.sync_split_dimming();
    terminal_tab_state.trigger_rebuild(false);
    (internals.callbacks.on_state_changed)();
    let cwd = new_leaf.cwd.borrow().clone();

    Ok(SurfaceSummary {
        pane_id: internals.pane_id,
        surface_id: new_surface_id,
        title: title_label.label().to_string(),
        kind: "terminal".to_string(),
        selected: false,
        cwd,
    })
}

pub fn run_command_in_terminal_tab(
    pane_widget: &gtk::Widget,
    tab_id: &str,
    source_surface_id: &str,
    surface_id: &str,
    command: &str,
) -> Result<(), TerminalTabSurfaceError> {
    let (internals, terminal_tab_state, _) = terminal_tab_context(pane_widget, tab_id)?;
    let handle = {
        let tree = terminal_tab_state.inner.tree.borrow();
        let source = tree
            .find_surface(internals.pane_id, tab_id, source_surface_id)
            .ok_or(TerminalTabSurfaceError::SourceNotFound)?;
        let target = tree
            .find_surface(internals.pane_id, tab_id, surface_id)
            .ok_or(TerminalTabSurfaceError::SurfaceNotFound)?;
        if target.creator_surface_id.as_deref() != Some(source.surface_id.as_str()) {
            return Err(TerminalTabSurfaceError::SurfaceNotOwned);
        }
        target.handle.clone()
    };
    if !handle.send_text(&format!("{command}\n")) {
        return Err(TerminalTabSurfaceError::CommandNotWritable);
    }
    Ok(())
}

pub fn close_surface_in_terminal_tab(
    pane_widget: &gtk::Widget,
    tab_id: &str,
    source_surface_id: &str,
    surface_id: &str,
) -> Result<(), TerminalTabSurfaceError> {
    let (internals, terminal_tab_state, _) = terminal_tab_context(pane_widget, tab_id)?;
    let (leaf_id, handle) = {
        let tree = terminal_tab_state.inner.tree.borrow();
        let source = tree
            .find_surface(internals.pane_id, tab_id, source_surface_id)
            .ok_or(TerminalTabSurfaceError::SourceNotFound)?;
        let target = tree
            .find_surface(internals.pane_id, tab_id, surface_id)
            .ok_or(TerminalTabSurfaceError::SurfaceNotFound)?;
        if target.leaf_id == source.leaf_id {
            return Err(TerminalTabSurfaceError::CannotCloseSource);
        }
        if target.creator_surface_id.as_deref() != Some(source.surface_id.as_str()) {
            return Err(TerminalTabSurfaceError::SurfaceNotOwned);
        }
        (target.leaf_id.clone(), target.handle.clone())
    };
    if !terminal_tab_state.close_leaf(&leaf_id) {
        return Err(TerminalTabSurfaceError::SurfaceNotFound);
    }
    handle.close();
    (internals.callbacks.on_state_changed)();
    Ok(())
}

pub fn focus_active_terminal_in_pane(
    pane_widget: &gtk::Widget,
    direction: TerminalFocusDirection,
) -> bool {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return false;
    };
    let terminal_tab_state = {
        let tab_state = internals.tab_state.borrow();
        let active_id = tab_state
            .active_tab
            .clone()
            .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()));
        let Some(active_id) = active_id else {
            return false;
        };
        let Some(entry) = tab_state.tabs.iter().find(|entry| entry.id == active_id) else {
            return false;
        };
        let TabKind::Terminal { state } = &entry.kind else {
            return false;
        };
        state.clone()
    };
    let active_leaf_id = terminal_tab_state.active_leaf_id();
    let mut leaves = Vec::new();
    terminal_tab_state
        .inner
        .tree
        .borrow()
        .for_each_leaf(|leaf| leaves.push(leaf.clone()));
    let Some(target_index) = leaves
        .iter()
        .position(|leaf| leaf.leaf_id == active_leaf_id)
        .and_then(|index| terminal_focus_index(index, leaves.len(), direction))
    else {
        return false;
    };
    let target = &leaves[target_index];
    if !terminal_tab_state.set_active_leaf(&target.leaf_id) {
        return false;
    }
    target.handle.focus_surface();
    true
}

fn make_terminal_callbacks(
    internals: &Rc<PaneInternals>,
    terminal_tab_state: &TerminalTabState,
    tab_id: &str,
    title_label: &gtk::Label,
    leaf: &TerminalLeafState,
) -> TerminalCallbacks {
    *terminal_tab_state.inner.on_state_changed.borrow_mut() =
        Some(Rc::downgrade(&internals.callbacks));
    let tid_for_title = tab_id.to_string();
    let leaf_id = leaf.leaf_id.clone();
    let title_label_for_title = title_label.clone();
    let state_for_title = internals.tab_state.clone();
    let callbacks_for_bell = internals.callbacks.clone();
    let callbacks_for_pwd = internals.callbacks.clone();
    let callbacks_for_close = internals.callbacks.clone();
    let tab_strip = internals.tab_strip.clone();
    let content_stack = internals.content_stack.clone();
    let tab_state = internals.tab_state.clone();
    let term_cwd_for_pwd = leaf.cwd.clone();
    let tid_for_close = tab_id.to_string();
    let tid_for_notification = tab_id.to_string();
    let pane_id = internals.pane_id;
    let terminal_tab_state_for_focus = terminal_tab_state.clone();
    let terminal_tab_state_for_close = terminal_tab_state.clone();
    let terminal_tab_state_for_within_right = terminal_tab_state.clone();
    let terminal_tab_state_for_within_down = terminal_tab_state.clone();
    let source_leaf_for_within = leaf.clone();

    TerminalCallbacks {
        on_title_changed: Box::new(move |title: &str| {
            let has_custom = state_for_title
                .borrow()
                .tabs
                .iter()
                .any(|entry| entry.id == tid_for_title && entry.custom_name.is_some());
            if has_custom || title.is_empty() {
                return;
            }
            let mut chars = title.char_indices();
            let display = match chars.nth(21) {
                Some((end, _)) if chars.next().is_some() => format!("{}…", &title[..end]),
                _ => title.to_string(),
            };
            title_label_for_title.set_label(&display);
        }),
        on_pwd_changed: Box::new(move |pwd: &str| {
            *term_cwd_for_pwd.borrow_mut() = Some(pwd.to_string());
            (callbacks_for_pwd.on_pwd_changed)(pwd);
            (callbacks_for_pwd.on_state_changed)();
        }),
        on_focus: Box::new({
            let callbacks = internals.callbacks.clone();
            let leaf_id = leaf_id.clone();
            move || {
                if terminal_tab_state_for_focus.set_active_leaf(&leaf_id) {
                    (callbacks.on_state_changed)();
                }
            }
        }),
        on_desktop_notification: Box::new({
            let callbacks = internals.callbacks.clone();
            let tab_id = tid_for_notification.clone();
            move |title: &str, body: &str, source_focused: bool| {
                (callbacks.on_desktop_notification)(title, body, source_focused, pane_id, &tab_id);
            }
        }),
        on_bell: Box::new({
            let tab_id = tid_for_notification.clone();
            move |source_focused| {
                (callbacks_for_bell.on_bell)(source_focused, pane_id, &tab_id);
            }
        }),
        on_close: Box::new(move || {
            let tab_strip = tab_strip.clone();
            let content_stack = content_stack.clone();
            let tab_state = tab_state.clone();
            let callbacks = callbacks_for_close.clone();
            let tab_id = tid_for_close.clone();
            let terminal_tab_state = terminal_tab_state_for_close.clone();
            let leaf_id = leaf_id.clone();
            glib::idle_add_local_once(move || {
                if !terminal_tab_state.has_leaf(&leaf_id) {
                    return;
                }
                if terminal_tab_state.close_leaf(&leaf_id) {
                    (callbacks.on_state_changed)();
                    return;
                }
                remove_tab(&tab_strip, &content_stack, &tab_state, &tab_id, &callbacks);
            });
        }),
        on_open_url: Box::new(|url, _| open_url_in_external_browser(url)),
        on_swap: Box::new({
            let state = terminal_tab_state.clone();
            let leaf_id = leaf.leaf_id.clone();
            move || state.start_swap(&leaf_id)
        }),
        on_split_right: Box::new({
            let internals = internals.clone();
            let title_label = title_label.clone();
            let tab_id = tab_id.to_string();
            let source_leaf = source_leaf_for_within.clone();
            move || {
                let internals = internals.clone();
                let terminal_tab_state = terminal_tab_state_for_within_right.clone();
                let tab_id = tab_id.clone();
                let title_label = title_label.clone();
                let source_leaf = source_leaf.clone();
                glib::idle_add_local_once(move || {
                    let _ = split_terminal_tab_leaf(
                        &internals,
                        &terminal_tab_state,
                        &tab_id,
                        &title_label,
                        &source_leaf,
                        gtk::Orientation::Horizontal,
                    );
                });
            }
        }),
        on_split_down: Box::new({
            let internals = internals.clone();
            let title_label = title_label.clone();
            let tab_id = tab_id.to_string();
            let source_leaf = leaf.clone();
            move || {
                let internals = internals.clone();
                let terminal_tab_state = terminal_tab_state_for_within_down.clone();
                let tab_id = tab_id.clone();
                let title_label = title_label.clone();
                let source_leaf = source_leaf.clone();
                glib::idle_add_local_once(move || {
                    let _ = split_terminal_tab_leaf(
                        &internals,
                        &terminal_tab_state,
                        &tab_id,
                        &title_label,
                        &source_leaf,
                        gtk::Orientation::Vertical,
                    );
                });
            }
        }),
    }
}

fn open_url_in_external_browser(url: &str) {
    if let Err(err) =
        gtk::gio::AppInfo::launch_default_for_uri(url, None::<&gtk::gio::AppLaunchContext>)
    {
        eprintln!("ghostdeck: failed to open URL in external browser: {err}");
    }
}

fn add_terminal_tab_inner(
    internals: &Rc<PaneInternals>,
    working_directory: Option<&str>,
    options: Option<TerminalTabOptions<'_>>,
) {
    let tab_id = options
        .as_ref()
        .and_then(|value| value.id.map(|id| id.to_string()))
        .unwrap_or_else(next_tab_id);
    let (tab_btn, title_label) = build_tab_button("Terminal", &tab_id, internals);
    let tree = options
        .as_ref()
        .and_then(|value| value.tree)
        .map(|tree| runtime_terminal_tree_from_layout(internals, &tab_id, working_directory, tree))
        .unwrap_or_else(|| {
            TerminalSplitNode::Leaf(create_terminal_leaf(
                internals,
                &tab_id,
                "leaf-0",
                working_directory,
                options.as_ref().and_then(|value| value.cwd),
                options.as_ref().and_then(|value| value.agent.clone()),
            ))
        });
    let state = TerminalTabState::from_tree(
        tree,
        options
            .as_ref()
            .and_then(|value| value.active_leaf_id.map(|value| value.to_string())),
    );
    let callback_state = state.clone();
    state.replace_callbacks(|leaf| {
        make_terminal_callbacks(internals, &callback_state, &tab_id, &title_label, leaf)
    });
    let widget = state.root();
    internals.content_stack.add_named(&widget, Some(&tab_id));

    {
        let mut ts = internals.tab_state.borrow_mut();
        ts.tabs.push(TabEntry {
            id: tab_id.clone(),
            tab_button: tab_btn,
            title_label: title_label.clone(),
            content: widget,
            custom_name: options
                .as_ref()
                .and_then(|value| value.custom_name.map(|name| name.to_string())),
            pinned: options.as_ref().map(|value| value.pinned).unwrap_or(false),
            kind: TabKind::Terminal {
                state: state.clone(),
            },
        });
    }
    internals.tab_strip.append(
        &internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
            .expect("terminal tab inserted")
            .tab_button,
    );

    if let Some(custom_name) = options.as_ref().and_then(|value| value.custom_name) {
        title_label.set_label(custom_name);
    }
    if options.as_ref().map(|value| value.pinned).unwrap_or(false) {
        if let Some(entry) = internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
        {
            apply_pin_visuals(&entry.tab_button, true);
        }
    }

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &tab_id,
    );
    state.active_handle().focus_surface();
    if options.is_none() {
        (internals.callbacks.on_state_changed)();
    }
}

fn add_keybind_editor_tab_inner(internals: &Rc<PaneInternals>, input: KeybindsTabInput<'_>) {
    let tab_id = input
        .options
        .as_ref()
        .and_then(|value| value.id.map(|id| id.to_string()))
        .unwrap_or_else(next_tab_id);

    let (tab_btn, title_label) = build_tab_button("Keybinds", &tab_id, internals);

    let widget = keybind_editor::build_keybind_editor(&input.shortcuts, input.on_capture);
    internals.content_stack.add_named(&widget, Some(&tab_id));

    {
        let mut ts = internals.tab_state.borrow_mut();
        ts.tabs.push(TabEntry {
            id: tab_id.clone(),
            tab_button: tab_btn,
            title_label: title_label.clone(),
            content: widget,
            custom_name: input
                .options
                .as_ref()
                .and_then(|value| value.custom_name.map(|name| name.to_string())),
            pinned: input
                .options
                .as_ref()
                .map(|value| value.pinned)
                .unwrap_or(false),
            kind: TabKind::Keybinds,
        });
    }
    internals.tab_strip.append(
        &internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
            .expect("keybinds tab inserted")
            .tab_button,
    );

    if let Some(custom_name) = input.options.as_ref().and_then(|value| value.custom_name) {
        title_label.set_label(custom_name);
    }
    if input
        .options
        .as_ref()
        .map(|value| value.pinned)
        .unwrap_or(false)
    {
        if let Some(entry) = internals
            .tab_state
            .borrow()
            .tabs
            .iter()
            .find(|entry| entry.id == tab_id)
        {
            apply_pin_visuals(&entry.tab_button, true);
        }
    }

    activate_tab(
        &internals.tab_strip,
        &internals.content_stack,
        &internals.tab_state,
        &tab_id,
    );
    if input.options.is_none() {
        (internals.callbacks.on_state_changed)();
    }
}

#[allow(dead_code)]
pub fn add_terminal_tab_to_pane(pane_widget: &gtk::Widget) {
    if let Some(internals) = find_pane_internals(pane_widget) {
        let dir = internals.working_directory.borrow().clone();
        add_terminal_tab_inner(&internals, dir.as_deref(), None);
    }
}

pub fn refresh_shortcut_tooltips(pane_widget: &gtk::Widget, shortcuts: &ResolvedShortcutConfig) {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return;
    };

    internals
        .new_terminal_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "New terminal tab",
            Some(ShortcutId::NewTerminal),
        )));
    internals
        .split_right_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "Split right",
            Some(ShortcutId::SplitRight),
        )));
    internals
        .split_down_button
        .set_tooltip_text(Some(&pane_action_tooltip(
            shortcuts,
            "Split down",
            Some(ShortcutId::SplitDown),
        )));
}

pub fn snapshot_pane_state(pane_widget: &gtk::Widget) -> Option<PaneState> {
    let internals = find_pane_internals(pane_widget)?;
    let ts = internals.tab_state.borrow();
    let tabs = ts
        .tabs
        .iter()
        .map(|entry| {
            let content = match &entry.kind {
                TabKind::Terminal { state } => {
                    let tree = state.snapshot_tree();
                    TabContentState::Terminal {
                        cwd: state.active_cwd(),
                        agent: state.active_agent(),
                        tree: Some(Box::new(tree)),
                        active_leaf_id: Some(state.active_leaf_id()),
                    }
                }
                TabKind::Keybinds => TabContentState::Keybinds {},
            };
            SavedTabState {
                id: entry.id.clone(),
                custom_name: entry.custom_name.clone(),
                pinned: entry.pinned,
                content,
            }
        })
        .collect();
    Some(PaneState {
        pane_id: Some(internals.pane_id),
        active_tab_id: ts.active_tab.clone(),
        tabs,
    })
}

fn find_pane_internals(pane_widget: &gtk::Widget) -> Option<Rc<PaneInternals>> {
    let outer = pane_widget.downcast_ref::<gtk::Box>()?;
    unsafe {
        outer
            .data::<Rc<PaneInternals>>("ghostdeck-pane-internals")
            .map(|ptr| ptr.as_ref().clone())
    }
}

pub fn is_pane_widget(widget: &gtk::Widget) -> bool {
    let Some(container) = widget.downcast_ref::<gtk::Box>() else {
        return false;
    };

    let mut child = container.first_child();
    while let Some(current) = child {
        if current.has_css_class("ghostdeck-pane-header") {
            return true;
        }
        child = current.next_sibling();
    }

    false
}

pub fn tab_title(pane_widget: &gtk::Widget, tab_id: &str) -> Option<String> {
    let internals = find_pane_internals(pane_widget)?;
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state.tabs.iter().find(|entry| entry.id == tab_id)?;
    Some(entry.title_label.label().to_string())
}

pub fn tab_working_directory(pane_widget: &gtk::Widget, tab_id: &str) -> Option<String> {
    let internals = find_pane_internals(pane_widget)?;
    let tab_state = internals.tab_state.borrow();
    let entry = tab_state.tabs.iter().find(|entry| entry.id == tab_id)?;
    match &entry.kind {
        TabKind::Terminal { state } => state.active_cwd(),
        TabKind::Keybinds => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneSummary {
    pub pane_id: u32,
    pub surface_count: usize,
    pub active_surface_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSummary {
    pub pane_id: u32,
    pub surface_id: String,
    pub title: String,
    pub kind: String,
    pub selected: bool,
    pub cwd: Option<String>,
}

fn pane_internals_for_root(root: &gtk::Widget) -> Vec<Rc<PaneInternals>> {
    let mut panes = PANE_REGISTRY.with(|registry| {
        registry
            .borrow()
            .values()
            .filter_map(|weak| weak.upgrade())
            .filter(|internals| {
                internals.pane_outer.upcast_ref::<gtk::Widget>() == root
                    || internals.pane_outer.is_ancestor(root)
            })
            .collect::<Vec<_>>()
    });
    panes.sort_by_key(|internals| internals.pane_id);
    panes
}

pub fn pane_summaries_for_root(root: &gtk::Widget) -> Vec<PaneSummary> {
    pane_internals_for_root(root)
        .into_iter()
        .map(|internals| {
            let pane_id = internals.pane_id;
            let tab_state = internals.tab_state.borrow();
            let surface_count = tab_state
                .tabs
                .iter()
                .map(|entry| match &entry.kind {
                    TabKind::Terminal { state } => state.leaf_count(),
                    TabKind::Keybinds => 1,
                })
                .sum();
            let active_surface_id = tab_state
                .active_tab
                .as_deref()
                .and_then(|tab_id| {
                    tab_state
                        .tabs
                        .iter()
                        .find(|entry| entry.id == tab_id)
                        .map(|entry| match &entry.kind {
                            TabKind::Terminal { state } => {
                                terminal_surface_id(pane_id, &entry.id, &state.active_leaf_id())
                            }
                            TabKind::Keybinds => composite_surface_id(pane_id, &entry.id),
                        })
                })
                .or_else(|| {
                    tab_state.tabs.first().map(|entry| match &entry.kind {
                        TabKind::Terminal { state } => {
                            terminal_surface_id(pane_id, &entry.id, &state.active_leaf_id())
                        }
                        TabKind::Keybinds => composite_surface_id(pane_id, &entry.id),
                    })
                });
            PaneSummary {
                pane_id,
                surface_count,
                active_surface_id,
            }
        })
        .collect()
}

pub(crate) fn pane_widget_for_root(root: &gtk::Widget, pane_id: u32) -> Option<gtk::Widget> {
    pane_internals_for_root(root)
        .into_iter()
        .find(|internals| internals.pane_id == pane_id)
        .map(|internals| internals.pane_outer.clone().upcast())
}

pub fn surface_summaries_for_root(root: &gtk::Widget) -> Vec<SurfaceSummary> {
    let mut surfaces = Vec::new();

    for internals in pane_internals_for_root(root) {
        let pane_id = internals.pane_id;
        let tab_state = internals.tab_state.borrow();
        let active_tab = tab_state.active_tab.as_deref();
        for entry in &tab_state.tabs {
            let tab_selected = active_tab
                .map(|current| current == entry.id)
                .unwrap_or_else(|| {
                    tab_state
                        .tabs
                        .first()
                        .is_some_and(|first| first.id == entry.id)
                });
            match &entry.kind {
                TabKind::Terminal { state } => {
                    let active_leaf_id = state.active_leaf_id();
                    state.inner.tree.borrow().for_each_leaf(|leaf| {
                        surfaces.push(SurfaceSummary {
                            pane_id,
                            surface_id: terminal_surface_id(pane_id, &entry.id, &leaf.leaf_id),
                            title: entry.title_label.label().to_string(),
                            kind: "terminal".to_string(),
                            selected: tab_selected && leaf.leaf_id == active_leaf_id,
                            cwd: leaf.cwd.borrow().clone(),
                        });
                    });
                }
                TabKind::Keybinds => {
                    surfaces.push(SurfaceSummary {
                        pane_id,
                        surface_id: composite_surface_id(pane_id, &entry.id),
                        title: entry.title_label.label().to_string(),
                        kind: "keybinds".to_string(),
                        selected: tab_selected,
                        cwd: None,
                    });
                }
            }
        }
    }

    surfaces.sort_by(|left, right| {
        left.pane_id
            .cmp(&right.pane_id)
            .then_with(|| right.selected.cmp(&left.selected))
            .then_with(|| left.surface_id.cmp(&right.surface_id))
    });
    surfaces
}

pub fn active_surface_summary(pane_widget: &gtk::Widget) -> Option<SurfaceSummary> {
    let internals = find_pane_internals(pane_widget)?;
    let pane_id = internals.pane_id;
    let tab_state = internals.tab_state.borrow();
    let active_id = tab_state
        .active_tab
        .clone()
        .or_else(|| tab_state.tabs.first().map(|entry| entry.id.clone()))?;
    let entry = tab_state.tabs.iter().find(|entry| entry.id == active_id)?;
    Some(match &entry.kind {
        TabKind::Terminal { state } => SurfaceSummary {
            pane_id,
            surface_id: terminal_surface_id(pane_id, &entry.id, &state.active_leaf_id()),
            title: entry.title_label.label().to_string(),
            kind: "terminal".to_string(),
            selected: true,
            cwd: state.active_cwd(),
        },
        TabKind::Keybinds => SurfaceSummary {
            pane_id,
            surface_id: composite_surface_id(pane_id, &entry.id),
            title: entry.title_label.label().to_string(),
            kind: "keybinds".to_string(),
            selected: true,
            cwd: None,
        },
    })
}

pub fn terminal_handle_for_root(
    root: &gtk::Widget,
    surface_hint: Option<&str>,
) -> Option<(String, terminal::TerminalHandle)> {
    let requested = surface_hint
        .map(normalize_surface_hint)
        .filter(|value| !value.is_empty());

    if let Some(requested) = requested {
        for internals in pane_internals_for_root(root) {
            let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
            if let Some(found) = terminal_handle_for_surface(&pane_widget, Some(requested)) {
                return Some(found);
            }
        }
        return None;
    }

    pane_internals_for_root(root)
        .into_iter()
        .find_map(|internals| {
            let pane_widget: gtk::Widget = internals.pane_outer.clone().upcast();
            terminal_handle_for_surface(&pane_widget, None)
        })
}

pub fn move_tab_to_pane(
    source_pane: &gtk::Widget,
    tab_id: &str,
    target_pane: &gtk::Widget,
) -> bool {
    let Some(source) = find_pane_internals(source_pane) else {
        return false;
    };
    let Some(target) = find_pane_internals(target_pane) else {
        return false;
    };
    let insert_idx = target.tab_state.borrow().tabs.len();
    transfer_tab_between_panes(&source, &target, tab_id, insert_idx)
}

pub fn focused_shortcut_target(pane_widget: &gtk::Widget) -> FocusedShortcutTarget {
    let Some(internals) = find_pane_internals(pane_widget) else {
        return FocusedShortcutTarget::None;
    };

    let target = {
        let tab_state = internals.tab_state.borrow();
        let Some(active_id) = tab_state.active_tab.as_deref() else {
            return FocusedShortcutTarget::None;
        };
        match tab_state.tabs.iter().find(|entry| entry.id == active_id) {
            Some(TabEntry {
                kind: TabKind::Terminal { state },
                ..
            }) => FocusedShortcutTarget::Terminal(TerminalShortcutTarget {
                handle: state.active_handle(),
            }),
            Some(TabEntry {
                kind: TabKind::Keybinds,
                ..
            }) => FocusedShortcutTarget::Keybinds,
            None => FocusedShortcutTarget::None,
        }
    };

    target
}

fn apply_pin_visuals(tab_button: &gtk::Box, pinned: bool) {
    if let Some(close_widget) = tab_button.last_child() {
        close_widget.set_visible(!pinned);
    }
    if let Some(inner_box) = tab_button
        .first_child()
        .and_then(|child| child.downcast::<gtk::Box>().ok())
    {
        if let Some(pin_icon) = inner_box
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        {
            pin_icon.set_label(if pinned { "📌" } else { "" });
            pin_icon.set_visible(pinned);
        }
    }
}

// ---------------------------------------------------------------------------
// Tab button (label + close)
// ---------------------------------------------------------------------------

fn new_tab_title_label(title: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(title)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(20)
        .build();
    label.set_can_target(false);
    label
}

fn build_tab_button(
    title: &str,
    tab_id: &str,
    internals: &Rc<PaneInternals>,
) -> (gtk::Box, gtk::Label) {
    let label = new_tab_title_label(title);
    let tab_button = build_tab_button_from_label(&label, tab_id, internals);
    (tab_button, label)
}

fn build_tab_button_from_label(
    label: &gtk::Label,
    tab_id: &str,
    internals: &Rc<PaneInternals>,
) -> gtk::Box {
    if let Some(parent) = label
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Box>().ok())
    {
        parent.remove(label);
    }

    let pin_icon = gtk::Label::new(None);
    pin_icon.add_css_class("ghostdeck-pin-icon");
    pin_icon.set_visible(false);
    pin_icon.set_can_target(false);

    let close_btn = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .has_frame(false)
        .build();
    close_btn.add_css_class("ghostdeck-tab-close");

    let inner_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    inner_box.set_can_target(false);
    inner_box.append(&pin_icon);
    inner_box.append(label);

    let tab_btn = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab_btn.add_css_class("ghostdeck-tab");
    tab_btn.append(&inner_box);
    tab_btn.append(&close_btn);

    let click = gtk::GestureClick::new();
    click.set_button(1);
    {
        let tab_id = tab_id.to_string();
        let tab_strip = internals.tab_strip.clone();
        let content_stack = internals.content_stack.clone();
        let tab_state = internals.tab_state.clone();
        let callbacks = internals.callbacks.clone();
        click.connect_pressed(move |_, _, _, _| {
            activate_tab(&tab_strip, &content_stack, &tab_state, &tab_id);
            (callbacks.on_state_changed)();
        });
    }
    tab_btn.add_controller(click);

    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    {
        let tab_id = tab_id.to_string();
        let context = TabContextMenuContext {
            tab_strip: internals.tab_strip.clone(),
            content_stack: internals.content_stack.clone(),
            tab_state: internals.tab_state.clone(),
            callbacks: internals.callbacks.clone(),
            pane_outer: internals.pane_outer.clone(),
            label: label.clone(),
            pin_icon: pin_icon.clone(),
        };
        let tab_button = tab_btn.clone();
        right_click.connect_pressed(move |_, _, _, _| {
            show_tab_context_menu(&tab_button, &tab_id, &context);
        });
    }
    tab_btn.add_controller(right_click);

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gtk::gdk::DragAction::MOVE);
    {
        let tab_id = tab_id.to_string();
        let pane_id = internals.pane_id;
        drag_source.connect_prepare(move |_src, _x, _y| {
            let payload = glib::Value::from(&TabDragPayload::new(pane_id, &tab_id).encode());
            Some(gtk::gdk::ContentProvider::for_value(&payload))
        });
    }
    {
        let drop_indicator = internals.drop_indicator.clone();
        let tab_state = internals.tab_state.clone();
        drag_source.connect_drag_begin(move |source, _drag| {
            set_tab_dragging(true);
            if let Some(widget) = source.widget() {
                let allocation = widget.allocation();
                position_indicator(
                    &tab_state,
                    &drop_indicator,
                    (allocation.x() + allocation.width()) as f64,
                );
                let icon = gtk::WidgetPaintable::new(Some(&widget));
                source.set_icon(Some(&icon), 0, 0);
            }
        });
    }
    {
        let drop_indicator = internals.drop_indicator.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            set_tab_dragging(false);
            drop_indicator.set_visible(false);
        });
    }
    tab_btn.add_controller(drag_source);

    {
        let tab_id = tab_id.to_string();
        let tab_strip = internals.tab_strip.clone();
        let content_stack = internals.content_stack.clone();
        let tab_state = internals.tab_state.clone();
        let callbacks = internals.callbacks.clone();
        let pane_outer = internals.pane_outer.clone();
        close_btn.connect_clicked(move |_| {
            let is_pinned = tab_state
                .borrow()
                .tabs
                .iter()
                .any(|entry| entry.id == tab_id && entry.pinned);
            if !is_pinned {
                request_tab_close_confirmation(
                    &tab_strip,
                    &content_stack,
                    &tab_state,
                    &tab_id,
                    &callbacks,
                    &pane_outer,
                );
            }
        });
    }

    tab_btn
}

fn show_tab_context_menu(tab_btn: &gtk::Box, tab_id: &str, context: &TabContextMenuContext) {
    let menu = gtk::PopoverMenu::from_model(None::<&gtk::gio::MenuModel>);
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    menu_box.set_margin_top(4);
    menu_box.set_margin_bottom(4);
    menu_box.set_margin_start(4);
    menu_box.set_margin_end(4);

    // Rename
    let rename_btn = gtk::Button::with_label("Rename");
    rename_btn.add_css_class("flat");
    {
        let lbl = context.label.clone();
        let state = context.tab_state.clone();
        let tid = tab_id.to_string();
        let menu_ref = menu.clone();
        let callbacks = context.callbacks.clone();
        rename_btn.connect_clicked(move |_| {
            let hover_focus_guard = terminal::suspend_hover_focus();
            menu_ref.popdown();
            show_rename_dialog(&lbl, &state, &tid, &callbacks, hover_focus_guard);
        });
    }

    // Pin / Unpin
    let is_pinned = context
        .tab_state
        .borrow()
        .tabs
        .iter()
        .any(|e| e.id == tab_id && e.pinned);
    let pin_label = if is_pinned { "Unpin" } else { "Pin" };
    let pin_btn = gtk::Button::with_label(pin_label);
    pin_btn.add_css_class("flat");
    {
        let state = context.tab_state.clone();
        let tid = tab_id.to_string();
        let pin = context.pin_icon.clone();
        let close = tab_btn.last_child(); // close button
        let menu_ref = menu.clone();
        let callbacks = context.callbacks.clone();
        pin_btn.connect_clicked(move |_| {
            menu_ref.popdown();
            let mut ts = state.borrow_mut();
            if let Some(entry) = ts.find_tab_mut(&tid) {
                entry.pinned = !entry.pinned;
                apply_pin_visuals(&entry.tab_button, entry.pinned);
                pin.set_label(if entry.pinned { "📌" } else { "" });
                pin.set_visible(entry.pinned);
                if let Some(close_widget) = &close {
                    close_widget.set_visible(!entry.pinned);
                }
            }
            drop(ts);
            (callbacks.on_state_changed)();
        });
    }

    // Close
    let close_btn = gtk::Button::with_label("Close");
    close_btn.add_css_class("flat");
    {
        let tid = tab_id.to_string();
        let ts = context.tab_strip.clone();
        let cs = context.content_stack.clone();
        let state = context.tab_state.clone();
        let cb = context.callbacks.clone();
        let po = context.pane_outer.clone();
        let menu_ref = menu.clone();
        close_btn.connect_clicked(move |_| {
            menu_ref.popdown();
            request_tab_close_confirmation(&ts, &cs, &state, &tid, &cb, &po);
        });
    }

    menu_box.append(&rename_btn);
    menu_box.append(&pin_btn);
    menu_box.append(&close_btn);
    menu.set_child(Some(&menu_box));
    menu.set_parent(tab_btn);
    menu.set_has_arrow(false);

    // Clean up popover when it closes
    menu.connect_closed(move |popover| {
        popover.unparent();
    });

    menu.popup();
}

fn show_rename_dialog(
    label: &gtk::Label,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
    callbacks: &Rc<PaneCallbacks>,
    hover_focus_guard: terminal::HoverFocusGuard,
) {
    let current_name = label.label().to_string();

    // Replace label with an entry temporarily
    let parent = label.parent().and_then(|p| p.downcast::<gtk::Box>().ok());
    let Some(parent) = parent else {
        return;
    };

    let entry = gtk::Entry::builder()
        .text(&current_name)
        .width_chars(15)
        .build();
    for css_class in TAB_RENAME_ENTRY_CSS_CLASSES {
        entry.add_css_class(css_class);
    }

    label.set_visible(false);
    // Insert entry before the close button
    parent.insert_child_after(&entry, Some(label));
    entry.grab_focus();
    entry.select_region(0, -1);

    // On activate (Enter) or blur, commit rename.
    let lbl = label.clone();
    let state = tab_state.clone();
    let tid = tab_id.to_string();
    let parent_for_cleanup = parent.clone();

    let commit = Rc::new(std::cell::Cell::new(false));
    let hover_focus_guard = Rc::new(RefCell::new(Some(hover_focus_guard)));

    let do_rename = {
        let commit = commit.clone();
        let lbl = lbl.clone();
        let state = state.clone();
        let tid = tid.clone();
        let parent = parent_for_cleanup.clone();
        let callbacks = callbacks.clone();
        let hover_focus_guard = hover_focus_guard.clone();
        move |entry: &gtk::Entry| {
            if commit.get() {
                return;
            }
            commit.set(true);
            let new_name = entry.text().to_string();
            if !new_name.trim().is_empty() {
                lbl.set_label(&new_name);
                let mut ts = state.borrow_mut();
                if let Some(tab) = ts.find_tab_mut(&tid) {
                    tab.custom_name = Some(new_name);
                }
            }
            lbl.set_visible(true);
            parent.remove(entry);
            hover_focus_guard.borrow_mut().take();
            (callbacks.on_state_changed)();
        }
    };

    {
        let do_rename = do_rename.clone();
        entry.connect_activate(move |e| {
            do_rename(e);
        });
    }
    {
        let do_rename = do_rename.clone();
        entry.connect_notify_local(Some("has-focus"), move |entry, _| {
            if entry.has_focus() {
                return;
            }
            let do_rename = do_rename.clone();
            let entry = entry.clone();
            glib::idle_add_local_once(move || {
                if !entry.has_focus() {
                    do_rename(&entry);
                }
            });
        });
    }
}

fn normalize_reorder_insert_index(source_idx: usize, insert_idx: usize) -> Option<usize> {
    if source_idx == insert_idx || source_idx + 1 == insert_idx {
        return None;
    }
    Some(if source_idx < insert_idx {
        insert_idx - 1
    } else {
        insert_idx
    })
}

fn next_active_after_tab_removal(
    tab_ids: &[&str],
    active_id: Option<&str>,
    removed_idx: usize,
) -> Option<String> {
    if tab_ids.len() <= 1 {
        return None;
    }
    let removed_id = tab_ids.get(removed_idx).copied()?;
    if active_id != Some(removed_id) {
        return active_id.map(ToOwned::to_owned);
    }
    let next_idx = removed_idx.min(tab_ids.len() - 2);
    tab_ids
        .iter()
        .enumerate()
        .find_map(|(idx, tab_id)| (idx != removed_idx).then_some(*tab_id))
        .and_then(|_| {
            tab_ids
                .iter()
                .enumerate()
                .filter_map(|(idx, tab_id)| (idx != removed_idx).then_some(*tab_id))
                .nth(next_idx)
        })
        .map(ToOwned::to_owned)
}

fn position_indicator(tab_state: &Rc<RefCell<TabState>>, indicator: &gtk::Box, x: f64) {
    let tab_state = tab_state.borrow();
    if tab_state.tabs.is_empty() {
        indicator.set_visible(false);
        return;
    }

    let mut position = 0;
    for entry in &tab_state.tabs {
        let allocation = entry.tab_button.allocation();
        let left = allocation.x();
        let right = allocation.x() + allocation.width();
        let midpoint = allocation.x() as f64 + allocation.width() as f64 / 2.0;
        if x < midpoint {
            position = left;
            break;
        }
        position = right;
    }
    indicator.set_margin_start(position);
    indicator.set_visible(true);
}

fn insert_index_for_drop(
    tab_state: &Rc<RefCell<TabState>>,
    x: f64,
    ignored_tab_id: Option<&str>,
) -> usize {
    let tab_state = tab_state.borrow();
    for (idx, entry) in tab_state.tabs.iter().enumerate() {
        if ignored_tab_id == Some(entry.id.as_str()) {
            continue;
        }
        let allocation = entry.tab_button.allocation();
        let midpoint = allocation.x() as f64 + allocation.width() as f64 / 2.0;
        if x < midpoint {
            return idx;
        }
    }
    tab_state.tabs.len()
}

fn rebuild_tab_strip(tab_strip: &gtk::Box, tab_state: &Rc<RefCell<TabState>>) {
    let buttons: Vec<gtk::Box> = tab_state
        .borrow()
        .tabs
        .iter()
        .map(|entry| entry.tab_button.clone())
        .collect();
    for button in &buttons {
        if button.parent().is_some() {
            tab_strip.remove(button);
        }
    }
    for button in &buttons {
        tab_strip.append(button);
    }
}

fn rebind_moved_tab_entry(entry: &mut TabEntry, target: &Rc<PaneInternals>) {
    if let TabKind::Terminal { state } = &entry.kind {
        let callback_state = state.clone();
        state.replace_callbacks(|leaf| {
            make_terminal_callbacks(target, &callback_state, &entry.id, &entry.title_label, leaf)
        });
    }
    entry.tab_button = build_tab_button_from_label(&entry.title_label, &entry.id, target);
    if entry.pinned {
        apply_pin_visuals(&entry.tab_button, true);
    }
}

fn reorder_tab_to_index(
    tab_strip: &gtk::Box,
    tab_state: &Rc<RefCell<TabState>>,
    callbacks: &Rc<PaneCallbacks>,
    source_id: &str,
    insert_idx: usize,
) -> bool {
    let mut state = tab_state.borrow_mut();
    let Some(source_idx) = state.tabs.iter().position(|entry| entry.id == source_id) else {
        return false;
    };
    let Some(normalized_idx) = normalize_reorder_insert_index(source_idx, insert_idx) else {
        return false;
    };
    let entry = state.tabs.remove(source_idx);
    state.tabs.insert(normalized_idx, entry);
    drop(state);
    rebuild_tab_strip(tab_strip, tab_state);
    (callbacks.on_state_changed)();
    true
}

fn transfer_tab_between_panes(
    source: &Rc<PaneInternals>,
    target: &Rc<PaneInternals>,
    tab_id: &str,
    insert_idx: usize,
) -> bool {
    if source.pane_id == target.pane_id {
        return false;
    }

    let (mut entry, source_next_active) = {
        let mut source_state = source.tab_state.borrow_mut();
        let Some(source_idx) = source_state.tabs.iter().position(|item| item.id == tab_id) else {
            return false;
        };
        let all_ids: Vec<&str> = source_state
            .tabs
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let next_active =
            next_active_after_tab_removal(&all_ids, source_state.active_tab.as_deref(), source_idx);
        (source_state.tabs.remove(source_idx), next_active)
    };

    if let Some(window) = entry
        .content
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok())
    {
        gtk::prelude::GtkWindowExt::set_focus(&window, gtk::Widget::NONE);
    }

    if entry.tab_button.parent().is_some() {
        source.tab_strip.remove(&entry.tab_button);
    }
    if entry.content.parent().is_some() {
        source.content_stack.remove(&entry.content);
    }

    rebind_moved_tab_entry(&mut entry, target);
    let moved_tab_id = entry.id.clone();
    target
        .content_stack
        .add_named(&entry.content, Some(&moved_tab_id));

    {
        let mut target_state = target.tab_state.borrow_mut();
        let clamped_idx = insert_idx.min(target_state.tabs.len());
        target_state.tabs.insert(clamped_idx, entry);
    }
    rebuild_tab_strip(&target.tab_strip, &target.tab_state);

    let source_empty = source.tab_state.borrow().tabs.is_empty();
    if source_empty {
        (source.callbacks.on_empty)();
    } else if let Some(next_active) = source_next_active {
        activate_tab(
            &source.tab_strip,
            &source.content_stack,
            &source.tab_state,
            &next_active,
        );
    }

    activate_tab(
        &target.tab_strip,
        &target.content_stack,
        &target.tab_state,
        &moved_tab_id,
    );
    (target.callbacks.on_state_changed)();
    true
}

fn install_tab_strip_drop_target(tab_overlay: &gtk::Overlay, internals: &Rc<PaneInternals>) {
    let drop_target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
    drop_target.set_preload(true);
    {
        let tab_state = internals.tab_state.clone();
        let indicator = internals.drop_indicator.clone();
        let workspace_dragging = internals.workspace_dragging.clone();
        drop_target.connect_motion(move |_, x, _| {
            if workspace_dragging.get() || !is_tab_dragging() {
                indicator.set_visible(false);
                return gtk::gdk::DragAction::empty();
            }
            position_indicator(&tab_state, &indicator, x);
            gtk::gdk::DragAction::MOVE
        });
    }
    {
        let indicator = internals.drop_indicator.clone();
        drop_target.connect_leave(move |_| {
            indicator.set_visible(false);
        });
    }
    {
        let target = internals.clone();
        let indicator = internals.drop_indicator.clone();
        drop_target.connect_drop(move |_, value, x, _| {
            indicator.set_visible(false);
            let Ok(raw) = value.get::<String>() else {
                return false;
            };
            let Some(payload) = TabDragPayload::decode(&raw) else {
                return false;
            };
            let same_pane = payload.pane_id == target.pane_id;
            let insert_idx = insert_index_for_drop(
                &target.tab_state,
                x,
                same_pane.then_some(payload.tab_id.as_str()),
            );
            if same_pane {
                return reorder_tab_to_index(
                    &target.tab_strip,
                    &target.tab_state,
                    &target.callbacks,
                    &payload.tab_id,
                    insert_idx,
                );
            }
            let Some(source) = lookup_pane_internals(payload.pane_id) else {
                return false;
            };
            transfer_tab_between_panes(&source, &target, &payload.tab_id, insert_idx)
        });
    }
    tab_overlay.add_controller(drop_target);
}

// ---------------------------------------------------------------------------
// Tab activation / removal
// ---------------------------------------------------------------------------

fn activate_tab(
    _tab_strip: &gtk::Box,
    content_stack: &gtk::Stack,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
) {
    let mut ts = tab_state.borrow_mut();
    if ts.active_tab.as_deref() != Some(tab_id) {
        if let Some(TabKind::Terminal { state }) = ts
            .tabs
            .iter()
            .find(|entry| Some(entry.id.as_str()) == ts.active_tab.as_deref())
            .map(|entry| &entry.kind)
        {
            state.cancel_swap();
        }
    }
    ts.active_tab = Some(tab_id.to_string());

    // Update visual state on all tabs
    for entry in &ts.tabs {
        if entry.id == tab_id {
            entry.tab_button.add_css_class("ghostdeck-tab-active");
        } else {
            entry.tab_button.remove_css_class("ghostdeck-tab-active");
        }
    }

    if content_stack.child_by_name(tab_id).is_some() {
        content_stack.set_visible_child_name(tab_id);
    }

    let focus_target = ts
        .tabs
        .iter()
        .find(|entry| entry.id == tab_id)
        .map(TabFocusTarget::from_entry);
    drop(ts);

    if let Some(target) = focus_target {
        // Mouse-initiated tab switches can leave focus on the click target if we
        // refocus synchronously. Deferring to the next idle tick makes the newly
        // active surface the final focus owner.
        glib::idle_add_local_once(move || {
            target.focus();
        });
    }
}

fn request_tab_close_confirmation(
    tab_strip: &gtk::Box,
    content_stack: &gtk::Stack,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
    callbacks: &Rc<PaneCallbacks>,
    pane_outer: &gtk::Box,
) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message("Close this tab?")
        .detail("Any running process or unsaved page state in this tab will be lost.")
        .build();
    dialog.set_buttons(&["Cancel", "Close Tab"]);
    dialog.set_default_button(0);
    dialog.set_cancel_button(0);

    let tab_strip = tab_strip.clone();
    let content_stack = content_stack.clone();
    let tab_state = tab_state.clone();
    let tab_id = tab_id.to_string();
    let callbacks = callbacks.clone();
    let pane_outer = pane_outer.clone();
    let window = pane_outer
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok());
    dialog.choose(
        window.as_ref(),
        None::<&gtk::gio::Cancellable>,
        move |response| {
            if response.ok() == Some(1) {
                remove_tab(&tab_strip, &content_stack, &tab_state, &tab_id, &callbacks);
            }
        },
    );
}

fn remove_tab(
    tab_strip: &gtk::Box,
    content_stack: &gtk::Stack,
    tab_state: &Rc<RefCell<TabState>>,
    tab_id: &str,
    callbacks: &Rc<PaneCallbacks>,
) {
    let mut ts = tab_state.borrow_mut();
    let Some(idx) = ts.tabs.iter().position(|e| e.id == tab_id) else {
        return;
    };
    let entry = ts.tabs.remove(idx);
    if let TabKind::Terminal { state } = &entry.kind {
        state
            .inner
            .tree
            .borrow()
            .for_each_leaf(|leaf| leaf.handle.close());
    }

    tab_strip.remove(&entry.tab_button);
    content_stack.remove(&entry.content);

    if ts.tabs.is_empty() {
        drop(ts);
        (callbacks.on_empty)();
        return;
    }

    // Activate neighbor tab
    let new_idx = idx.min(ts.tabs.len() - 1);
    let new_id = ts.tabs[new_idx].id.clone();
    let was_active = ts.active_tab.as_deref() == Some(tab_id);
    drop(ts);

    if was_active {
        activate_tab(tab_strip, content_stack, tab_state, &new_id);
    }
    (callbacks.on_state_changed)();
}

#[cfg(test)]
mod tests {
    use super::{
        next_active_after_tab_removal, normalize_reorder_insert_index, pane_action_tooltip,
        resolve_terminal_working_directory, surface_hint_matches, terminal_focus_index,
        TabDragPayload, TerminalFocusDirection,
    };
    use crate::shortcut_config::{default_shortcuts, resolve_shortcuts_from_str, ShortcutId};

    #[test]
    fn terminal_leaf_cwd_overrides_workspace_fallback() {
        assert_eq!(
            resolve_terminal_working_directory(Some("/leaf"), Some("/workspace")),
            Some("/leaf")
        );
        assert_eq!(
            resolve_terminal_working_directory(None, Some("/workspace")),
            Some("/workspace")
        );
    }

    #[test]
    fn pane_action_tooltip_reflects_remaps_and_unbinds() {
        let defaults = default_shortcuts();
        assert_eq!(
            pane_action_tooltip(&defaults, "New terminal tab", Some(ShortcutId::NewTerminal)),
            "New terminal tab (Ctrl+T)"
        );

        let remapped = resolve_shortcuts_from_str(
            r#"{
                "shortcuts": {
                    "split_right": "<Ctrl><Alt>h"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            pane_action_tooltip(&remapped, "Split right", Some(ShortcutId::SplitRight)),
            "Split right (Ctrl+Alt+H)"
        );
    }

    #[test]
    fn terminal_focus_cycles_in_split_tree_order() {
        assert_eq!(
            terminal_focus_index(0, 4, TerminalFocusDirection::Right),
            Some(1)
        );
        assert_eq!(
            terminal_focus_index(1, 4, TerminalFocusDirection::Left),
            Some(0)
        );
        assert_eq!(
            terminal_focus_index(3, 4, TerminalFocusDirection::Right),
            Some(0)
        );
        assert_eq!(
            terminal_focus_index(0, 4, TerminalFocusDirection::Left),
            Some(3)
        );
        assert_eq!(
            terminal_focus_index(0, 1, TerminalFocusDirection::Right),
            None
        );
    }

    #[test]
    fn surface_hint_matches_only_exact_surface_or_tab_id() {
        assert!(surface_hint_matches(
            "42:tab-a:leaf-0",
            "42:tab-a",
            "tab-a",
            "surface:42:tab-a:leaf-0"
        ));
        assert!(surface_hint_matches(
            "42:tab-a:leaf-0",
            "42:tab-a",
            "tab-a",
            "42:tab-a"
        ));
        assert!(surface_hint_matches(
            "42:tab-a:leaf-0",
            "42:tab-a",
            "tab-a",
            "tab-a"
        ));
        assert!(!surface_hint_matches(
            "42:tab-a:leaf-0",
            "42:tab-a",
            "tab-a",
            "42:tab-b"
        ));
        assert!(!surface_hint_matches(
            "42:tab-a:leaf-0",
            "42:tab-a",
            "tab-a",
            ""
        ));
    }

    #[test]
    fn tab_drag_payload_round_trips() {
        let payload = TabDragPayload::new(17, "tab-123");
        let encoded = payload.encode();
        assert_eq!(encoded, "17:tab-123");
        assert_eq!(TabDragPayload::decode(&encoded), Some(payload));
    }

    #[test]
    fn tab_drag_payload_rejects_invalid_values() {
        assert_eq!(TabDragPayload::decode(""), None);
        assert_eq!(TabDragPayload::decode("17"), None);
        assert_eq!(TabDragPayload::decode("abc:tab"), None);
        assert_eq!(TabDragPayload::decode("17:"), None);
    }

    #[test]
    fn normalize_reorder_insert_index_adjusts_forward_moves() {
        assert_eq!(normalize_reorder_insert_index(1, 4), Some(3));
        assert_eq!(normalize_reorder_insert_index(4, 1), Some(1));
        assert_eq!(normalize_reorder_insert_index(2, 2), None);
        assert_eq!(normalize_reorder_insert_index(2, 3), None);
    }

    #[test]
    fn next_active_after_tab_removal_prefers_neighbor_when_active_removed() {
        assert_eq!(
            next_active_after_tab_removal(&["a", "b", "c"], Some("b"), 1),
            Some("c".to_string())
        );
        assert_eq!(
            next_active_after_tab_removal(&["a", "b", "c"], Some("a"), 0),
            Some("b".to_string())
        );
        assert_eq!(
            next_active_after_tab_removal(&["a", "b", "c"], Some("a"), 2),
            Some("a".to_string())
        );
        assert_eq!(
            next_active_after_tab_removal(&["only"], Some("only"), 0),
            None
        );
    }
}
