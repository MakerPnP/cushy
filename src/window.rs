//! Types for displaying a [`Widget`](crate::widget::Widget) inside of a desktop
//! window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::path::Path;
use std::string::ToString;
use std::sync::OnceLock;

use kludgine::app::winit::dpi::{PhysicalPosition, PhysicalSize};
use kludgine::app::winit::event::{
    DeviceId, ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, TouchPhase,
};
use kludgine::app::winit::keyboard::Key;
use kludgine::app::WindowBehavior as _;
use kludgine::figures::units::{Px, UPx};
use kludgine::figures::{IntoSigned, IntoUnsigned, Point, Rect, ScreenScale, Size};
use kludgine::render::Drawing;
use kludgine::Kludgine;
use tracing::Level;

use crate::context::{
    AsEventContext, EventContext, Exclusive, GraphicsContext, LayoutContext, RedrawStatus,
    WidgetContext,
};
use crate::graphics::Graphics;
use crate::styles::components::LayoutOrder;
use crate::styles::ThemePair;
use crate::tree::Tree;
use crate::utils::ModifiersExt;
use crate::value::{Dynamic, DynamicReader, IntoDynamic, Value};
use crate::widget::{
    EventHandling, ManagedWidget, Widget, WidgetId, WidgetInstance, HANDLED, IGNORED,
};
use crate::widgets::{Expand, Resize};
use crate::window::sealed::WindowCommand;
use crate::{initialize_tracing, ConstraintLimit, Run};

/// A currently running Gooey window.
pub struct RunningWindow<'window> {
    window: kludgine::app::Window<'window, WindowCommand>,
    focused: Dynamic<bool>,
    occluded: Dynamic<bool>,
}

impl<'window> RunningWindow<'window> {
    pub(crate) fn new(
        window: kludgine::app::Window<'window, WindowCommand>,
        focused: &Dynamic<bool>,
        occluded: &Dynamic<bool>,
    ) -> Self {
        Self {
            window,
            focused: focused.clone(),
            occluded: occluded.clone(),
        }
    }

    /// Returns a dynamic that is updated whenever this window's focus status
    /// changes.
    #[must_use]
    pub const fn focused(&self) -> &Dynamic<bool> {
        &self.focused
    }

    /// Returns a dynamic that is updated whenever this window's occlusion
    /// status changes.
    #[must_use]
    pub fn occluded(&self) -> &Dynamic<bool> {
        &self.occluded
    }
}

impl<'window> Deref for RunningWindow<'window> {
    type Target = kludgine::app::Window<'window, WindowCommand>;

    fn deref(&self) -> &Self::Target {
        &self.window
    }
}

impl<'window> DerefMut for RunningWindow<'window> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.window
    }
}

/// The attributes of a Gooey window.
pub type WindowAttributes = kludgine::app::WindowAttributes<WindowCommand>;

/// A Gooey window that is not yet running.
#[must_use]
pub struct Window<Behavior>
where
    Behavior: WindowBehavior,
{
    context: Behavior::Context,
    /// The attributes of this window.
    pub attributes: WindowAttributes,
    /// The colors to use to theme the user interface.
    pub theme: Value<ThemePair>,
    occluded: Option<Dynamic<bool>>,
    focused: Option<Dynamic<bool>>,
}

impl<Behavior> Default for Window<Behavior>
where
    Behavior: WindowBehavior,
    Behavior::Context: Default,
{
    fn default() -> Self {
        let context = Behavior::Context::default();
        Self::new(context)
    }
}

impl Window<WidgetInstance> {
    /// Returns a new instance using `widget` as its contents.
    pub fn for_widget<W>(widget: W) -> Self
    where
        W: Widget,
    {
        Self::new(WidgetInstance::new(widget))
    }

    /// Sets `focused` to be the dynamic updated when this window's focus status
    /// is changed.
    ///
    /// When the window is focused for user input, the dynamic will contain
    /// `true`.
    ///
    /// `focused` will be initialized with an initial state
    /// of `false`.
    pub fn with_focused(mut self, focused: impl IntoDynamic<bool>) -> Self {
        let focused = focused.into_dynamic();
        focused.update(false);
        self.focused = Some(focused);
        self
    }

    /// Sets `occluded` to be the dynamic updated when this window's occlusion
    /// status is changed.
    ///
    /// When the window is occluded (completely hidden/offscreen/minimized), the
    /// dynamic will contain `true`. If the window is at least partially
    /// visible, this value will contain `true`.
    ///
    /// `occluded` will be initialized with an initial state of `false`.
    pub fn with_occluded(mut self, occluded: impl IntoDynamic<bool>) -> Self {
        let occluded = occluded.into_dynamic();
        occluded.update(false);
        self.occluded = Some(occluded);
        self
    }
}

impl<Behavior> Window<Behavior>
where
    Behavior: WindowBehavior,
{
    /// Returns a new instance using `context` to initialize the window upon
    /// opening.
    pub fn new(context: Behavior::Context) -> Self {
        static EXECUTABLE_NAME: OnceLock<String> = OnceLock::new();

        let title = EXECUTABLE_NAME
            .get_or_init(|| {
                std::env::args_os()
                    .next()
                    .and_then(|path| {
                        Path::new(&path)
                            .file_name()
                            .and_then(OsStr::to_str)
                            .map(ToString::to_string)
                    })
                    .unwrap_or_else(|| String::from("Gooey App"))
            })
            .clone();
        Self {
            attributes: WindowAttributes {
                title,
                ..WindowAttributes::default()
            },
            context,
            theme: Value::default(),
            occluded: None,
            focused: None,
        }
    }
}

impl<Behavior> Run for Window<Behavior>
where
    Behavior: WindowBehavior,
{
    fn run(self) -> crate::Result {
        initialize_tracing();
        GooeyWindow::<Behavior>::run_with(AssertUnwindSafe(sealed::Context {
            user: self.context,
            settings: RefCell::new(sealed::WindowSettings {
                attributes: Some(self.attributes),
                occluded: self.occluded,
                focused: self.focused,
                theme: Some(self.theme),
            }),
        }))
    }
}

/// The behavior of a Gooey window.
pub trait WindowBehavior: Sized + 'static {
    /// The type that is provided when initializing this window.
    type Context: UnwindSafe + Send + 'static;

    /// Return a new instance of this behavior using `context`.
    fn initialize(window: &mut RunningWindow<'_>, context: Self::Context) -> Self;

    /// Create the window's root widget. This function is only invoked once.
    fn make_root(&mut self) -> WidgetInstance;

    /// The window has been requested to close. If this function returns true,
    /// the window will be closed. Returning false prevents the window from
    /// closing.
    #[allow(unused_variables)]
    fn close_requested(&self, window: &mut RunningWindow<'_>) -> bool {
        true
    }

    /// Runs this behavior as an application.
    fn run() -> crate::Result
    where
        Self::Context: Default,
    {
        Self::run_with(<Self::Context>::default())
    }

    /// Runs this behavior as an application, initialized with `context`.
    fn run_with(context: Self::Context) -> crate::Result {
        Window::<Self>::new(context).run()
    }
}

struct GooeyWindow<T> {
    behavior: T,
    root: ManagedWidget,
    contents: Drawing,
    should_close: bool,
    mouse_state: MouseState,
    redraw_status: RedrawStatus,
    initial_frame: bool,
    occluded: Dynamic<bool>,
    focused: Dynamic<bool>,
    keyboard_activated: Option<ManagedWidget>,
    min_inner_size: Option<Size<UPx>>,
    max_inner_size: Option<Size<UPx>>,
    theme: Option<DynamicReader<ThemePair>>,
    current_theme: ThemePair,
}

impl<T> GooeyWindow<T>
where
    T: WindowBehavior,
{
    fn request_close(&mut self, window: &mut RunningWindow<'_>) -> bool {
        self.should_close |= self.behavior.close_requested(window);

        self.should_close
    }

    fn keyboard_activate_widget(
        &mut self,
        is_pressed: bool,
        widget: Option<WidgetId>,
        window: &mut RunningWindow<'_>,
        kludgine: &mut Kludgine,
    ) {
        if is_pressed {
            if let Some(default) = widget.and_then(|id| self.root.tree.widget(id)) {
                if let Some(previously_active) = self.keyboard_activated.take() {
                    EventContext::new(
                        WidgetContext::new(
                            previously_active,
                            &self.redraw_status,
                            &self.current_theme,
                            window,
                        ),
                        kludgine,
                    )
                    .deactivate();
                }
                EventContext::new(
                    WidgetContext::new(
                        default.clone(),
                        &self.redraw_status,
                        &self.current_theme,
                        window,
                    ),
                    kludgine,
                )
                .activate();
                self.keyboard_activated = Some(default);
            }
        } else if let Some(keyboard_activated) = self.keyboard_activated.take() {
            EventContext::new(
                WidgetContext::new(
                    keyboard_activated,
                    &self.redraw_status,
                    &self.current_theme,
                    window,
                ),
                kludgine,
            )
            .deactivate();
        }
    }

    fn constrain_window_resizing(
        &mut self,
        resizable: bool,
        window: &kludgine::app::Window<'_, WindowCommand>,
        graphics: &mut kludgine::Graphics<'_>,
    ) -> bool {
        let mut root_or_child = self.root.widget.clone();
        let mut is_expanded = false;
        loop {
            let mut widget = root_or_child.lock();
            if let Some(resize) = widget.downcast_ref::<Resize>() {
                let min_width = resize
                    .width
                    .minimum()
                    .map_or(Px(0), |width| width.into_px(graphics.scale()));
                let max_width = resize
                    .width
                    .maximum()
                    .map_or(Px::MAX, |width| width.into_px(graphics.scale()));
                let min_height = resize
                    .height
                    .minimum()
                    .map_or(Px(0), |height| height.into_px(graphics.scale()));
                let max_height = resize
                    .height
                    .maximum()
                    .map_or(Px::MAX, |height| height.into_px(graphics.scale()));

                let new_min_size = (min_width > 0 || min_height > 0)
                    .then_some(Size::<Px>::new(min_width, min_height).into_unsigned());

                if new_min_size != self.min_inner_size {
                    window.set_min_inner_size(new_min_size);
                    self.min_inner_size = new_min_size;
                }
                let new_max_size = (max_width > 0 || max_height > 0)
                    .then_some(Size::<Px>::new(max_width, max_height).into_unsigned());

                if new_max_size != self.max_inner_size && resizable {
                    window.set_max_inner_size(new_max_size);
                }
                self.max_inner_size = new_max_size;
            } else if widget.downcast_ref::<Expand>().is_some() {
                is_expanded = true;
            }

            if let Some(wraps) = widget.as_widget().wraps().cloned() {
                drop(widget);

                root_or_child = wraps;
            } else {
                break;
            }
        }

        is_expanded
    }
}

impl<T> kludgine::app::WindowBehavior<WindowCommand> for GooeyWindow<T>
where
    T: WindowBehavior,
{
    type Context = AssertUnwindSafe<sealed::Context<T::Context>>;

    fn initialize(
        window: kludgine::app::Window<'_, WindowCommand>,
        _graphics: &mut kludgine::Graphics<'_>,
        AssertUnwindSafe(context): Self::Context,
    ) -> Self {
        let occluded = context
            .settings
            .borrow_mut()
            .occluded
            .take()
            .unwrap_or_default();
        let focused = context
            .settings
            .borrow_mut()
            .focused
            .take()
            .unwrap_or_default();
        let theme = context
            .settings
            .borrow_mut()
            .theme
            .take()
            .expect("theme always present");
        let mut behavior = T::initialize(
            &mut RunningWindow::new(window, &focused, &occluded),
            context.user,
        );
        let root = Tree::default().push_boxed(behavior.make_root(), None);

        let (current_theme, theme) = match theme {
            Value::Constant(theme) => (theme, None),
            Value::Dynamic(dynamic) => (dynamic.get(), Some(dynamic.into_reader())),
        };

        Self {
            behavior,
            root,
            contents: Drawing::default(),
            should_close: false,
            mouse_state: MouseState {
                location: None,
                widget: None,
                devices: HashMap::default(),
            },
            redraw_status: RedrawStatus::default(),
            initial_frame: true,
            occluded,
            focused,
            keyboard_activated: None,
            min_inner_size: None,
            max_inner_size: None,
            current_theme,
            theme,
        }
    }

    fn prepare(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        graphics: &mut kludgine::Graphics<'_>,
    ) {
        if let Some(theme) = &mut self.theme {
            if theme.has_updated() {
                self.current_theme = theme.get();
                // TODO invalidate everything, but right now we don't have much
                // cached. Maybe widgets should be told the theme has changed in
                // case some things like images have been cached.
            }
        }

        self.redraw_status.refresh_received();
        graphics.reset_text_attributes();
        self.root.tree.reset_render_order();

        let resizable = window.winit().is_resizable();
        let is_expanded = self.constrain_window_resizing(resizable, &window, graphics);

        let graphics = self.contents.new_frame(graphics);
        let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
        let mut context = GraphicsContext {
            widget: WidgetContext::new(
                self.root.clone(),
                &self.redraw_status,
                &self.current_theme,
                &mut window,
            ),
            gfx: Exclusive::Owned(Graphics::new(graphics)),
        };
        let mut layout_context = LayoutContext::new(&mut context);
        let window_size = layout_context.gfx.size();

        let background_color = layout_context.theme().surface.color;
        layout_context.graphics.gfx.fill(background_color);
        let actual_size = layout_context.layout(if is_expanded {
            Size::new(
                ConstraintLimit::Known(window_size.width),
                ConstraintLimit::Known(window_size.height),
            )
        } else {
            Size::new(
                ConstraintLimit::ClippedAfter(window_size.width),
                ConstraintLimit::ClippedAfter(window_size.height),
            )
        });
        let render_size = actual_size.min(window_size);
        if actual_size != window_size && !resizable {
            let mut new_size = actual_size;
            if let Some(min_size) = self.min_inner_size {
                new_size = new_size.max(min_size);
            }
            if let Some(max_size) = self.max_inner_size {
                new_size = new_size.min(max_size);
            }
            let _ = layout_context
                .winit()
                .request_inner_size(PhysicalSize::from(new_size));
        }
        self.root.set_layout(Rect::from(render_size.into_signed()));

        if self.initial_frame {
            self.initial_frame = false;
            self.root
                .lock()
                .as_widget()
                .mounted(&mut layout_context.as_event_context());
            layout_context.focus();
            layout_context.as_event_context().apply_pending_state();
        }

        if render_size.width < window_size.width || render_size.height < window_size.height {
            layout_context
                .clipped_to(Rect::from(render_size.into_signed()))
                .redraw();
        } else {
            layout_context.redraw();
        }
    }

    fn focus_changed(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        _kludgine: &mut Kludgine,
    ) {
        self.focused.update(window.focused());
    }

    fn occlusion_changed(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        _kludgine: &mut Kludgine,
    ) {
        self.occluded.update(window.ocluded());
    }

    fn render<'pass>(
        &'pass mut self,
        _window: kludgine::app::Window<'_, WindowCommand>,
        graphics: &mut kludgine::RenderingGraphics<'_, 'pass>,
    ) -> bool {
        self.contents.render(graphics);

        !self.should_close
    }

    fn initial_window_attributes(
        context: &Self::Context,
    ) -> kludgine::app::WindowAttributes<WindowCommand> {
        context
            .settings
            .borrow_mut()
            .attributes
            .take()
            .expect("called more than once")
    }

    fn close_requested(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        _kludgine: &mut Kludgine,
    ) -> bool {
        self.request_close(&mut RunningWindow::new(
            window,
            &self.focused,
            &self.occluded,
        ))
    }

    // fn power_preference() -> wgpu::PowerPreference {
    //     wgpu::PowerPreference::default()
    // }

    // fn limits(adapter_limits: wgpu::Limits) -> wgpu::Limits {
    //     wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter_limits)
    // }

    // fn clear_color() -> Option<kludgine::Color> {
    //     Some(kludgine::Color::BLACK)
    // }

    // fn focus_changed(&mut self, window: kludgine::app::Window<'_, ()>) {}

    // fn occlusion_changed(&mut self, window: kludgine::app::Window<'_, ()>) {}

    // fn scale_factor_changed(&mut self, window: kludgine::app::Window<'_, ()>) {}

    // fn resized(&mut self, window: kludgine::app::Window<'_, ()>) {}

    // fn theme_changed(&mut self, window: kludgine::app::Window<'_, ()>) {}

    // fn dropped_file(&mut self, window: kludgine::app::Window<'_, ()>, path: std::path::PathBuf) {}

    // fn hovered_file(&mut self, window: kludgine::app::Window<'_, ()>, path: std::path::PathBuf) {}

    // fn hovered_file_cancelled(&mut self, window: kludgine::app::Window<'_, ()>) {}

    // fn received_character(&mut self, window: kludgine::app::Window<'_, ()>, char: char) {}

    fn keyboard_input(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        kludgine: &mut Kludgine,
        device_id: DeviceId,
        input: KeyEvent,
        is_synthetic: bool,
    ) {
        let target = self.root.tree.focused_widget().unwrap_or(self.root.id());
        let target = self.root.tree.widget(target).expect("missing widget");
        let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
        let mut target = EventContext::new(
            WidgetContext::new(
                target,
                &self.redraw_status,
                &self.current_theme,
                &mut window,
            ),
            kludgine,
        );

        let handled = recursively_handle_event(&mut target, |widget| {
            widget.keyboard_input(device_id, input.clone(), is_synthetic)
        })
        .is_some();
        drop(target);

        if !handled {
            match input.logical_key {
                Key::Character(ch) if ch == "w" && window.modifiers().primary() => {
                    if input.state.is_pressed() && self.request_close(&mut window) {
                        window.set_needs_redraw();
                    }
                }
                Key::Tab if !window.modifiers().possible_shortcut() => {
                    if input.state.is_pressed() {
                        let reverse = window.modifiers().state().shift_key();

                        let target = self.root.tree.focused_widget().unwrap_or(self.root.id());
                        let target = self.root.tree.widget(target).expect("missing widget");
                        let mut target = EventContext::new(
                            WidgetContext::new(
                                target,
                                &self.redraw_status,
                                &self.current_theme,
                                &mut window,
                            ),
                            kludgine,
                        );
                        let mut visual_order = target.query_style(&LayoutOrder);
                        if reverse {
                            visual_order = visual_order.rev();
                        }
                        target.advance_focus(visual_order);
                    }
                }
                Key::Enter => {
                    self.keyboard_activate_widget(
                        input.state.is_pressed(),
                        self.root.tree.default_widget(),
                        &mut window,
                        kludgine,
                    );
                }
                Key::Escape => {
                    self.keyboard_activate_widget(
                        input.state.is_pressed(),
                        self.root.tree.escape_widget(),
                        &mut window,
                        kludgine,
                    );
                }
                _ => {
                    tracing::event!(
                        Level::DEBUG,
                        logical = ?input.logical_key,
                        physical = ?input.physical_key,
                        state = ?input.state,
                        "Ignored Keyboard Input",
                    );
                }
            }
        }
    }

    fn mouse_wheel(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        kludgine: &mut Kludgine,
        device_id: DeviceId,
        delta: MouseScrollDelta,
        phase: TouchPhase,
    ) {
        let widget = self
            .root
            .tree
            .hovered_widget()
            .and_then(|hovered| self.root.tree.widget(hovered))
            .unwrap_or_else(|| {
                self.root
                    .tree
                    .widget(self.root.id())
                    .expect("missing widget")
            });

        let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
        let mut widget = EventContext::new(
            WidgetContext::new(
                widget,
                &self.redraw_status,
                &self.current_theme,
                &mut window,
            ),
            kludgine,
        );
        recursively_handle_event(&mut widget, |widget| {
            widget.mouse_wheel(device_id, delta, phase)
        });
    }

    // fn modifiers_changed(&mut self, window: kludgine::app::Window<'_, ()>) {}

    fn ime(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        kludgine: &mut Kludgine,
        ime: Ime,
    ) {
        let widget = self
            .root
            .tree
            .focused_widget()
            .and_then(|hovered| self.root.tree.widget(hovered))
            .unwrap_or_else(|| {
                self.root
                    .tree
                    .widget(self.root.id())
                    .expect("missing widget")
            });
        let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
        let mut target = EventContext::new(
            WidgetContext::new(
                widget,
                &self.redraw_status,
                &self.current_theme,
                &mut window,
            ),
            kludgine,
        );

        let _handled =
            recursively_handle_event(&mut target, |widget| widget.ime(ime.clone())).is_some();
    }

    fn cursor_moved(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        kludgine: &mut Kludgine,
        device_id: DeviceId,
        position: PhysicalPosition<f64>,
    ) {
        let location = Point::<Px>::from(position);
        self.mouse_state.location = Some(location);

        let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
        if let Some(state) = self.mouse_state.devices.get(&device_id) {
            // Mouse Drag
            for (button, handler) in state {
                let mut context = EventContext::new(
                    WidgetContext::new(
                        handler.clone(),
                        &self.redraw_status,
                        &self.current_theme,
                        &mut window,
                    ),
                    kludgine,
                );
                let last_rendered_at = context.last_layout().expect("passed hit test");
                context.mouse_drag(location - last_rendered_at.origin, device_id, *button);
            }
        } else {
            // Hover
            let mut context = EventContext::new(
                WidgetContext::new(
                    self.root.clone(),
                    &self.redraw_status,
                    &self.current_theme,
                    &mut window,
                ),
                kludgine,
            );
            self.mouse_state.widget = None;
            for widget in self.root.tree.widgets_at_point(location) {
                let mut widget_context = context.for_other(&widget);
                let relative = location
                    - widget_context
                        .last_layout()
                        .expect("passed hit test")
                        .origin;

                if widget_context.hit_test(relative) {
                    widget_context.hover(relative);
                    drop(widget_context);
                    self.mouse_state.widget = Some(widget);
                    break;
                }
            }

            if self.mouse_state.widget.is_none() {
                context.clear_hover();
            }
        }
    }

    fn cursor_left(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        kludgine: &mut Kludgine,
        _device_id: DeviceId,
    ) {
        if self.mouse_state.widget.take().is_some() {
            let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
            let mut context = EventContext::new(
                WidgetContext::new(
                    self.root.clone(),
                    &self.redraw_status,
                    &self.current_theme,
                    &mut window,
                ),
                kludgine,
            );
            context.clear_hover();
        }
    }

    fn mouse_input(
        &mut self,
        window: kludgine::app::Window<'_, WindowCommand>,
        kludgine: &mut Kludgine,
        device_id: DeviceId,
        state: ElementState,
        button: MouseButton,
    ) {
        let mut window = RunningWindow::new(window, &self.focused, &self.occluded);
        match state {
            ElementState::Pressed => {
                EventContext::new(
                    WidgetContext::new(
                        self.root.clone(),
                        &self.redraw_status,
                        &self.current_theme,
                        &mut window,
                    ),
                    kludgine,
                )
                .clear_focus();

                if let (ElementState::Pressed, Some(location), Some(hovered)) =
                    (state, &self.mouse_state.location, &self.mouse_state.widget)
                {
                    if let Some(handler) = recursively_handle_event(
                        &mut EventContext::new(
                            WidgetContext::new(
                                hovered.clone(),
                                &self.redraw_status,
                                &self.current_theme,
                                &mut window,
                            ),
                            kludgine,
                        ),
                        |context| {
                            let relative =
                                *location - context.last_layout().expect("passed hit test").origin;
                            context.mouse_down(relative, device_id, button)
                        },
                    ) {
                        self.mouse_state
                            .devices
                            .entry(device_id)
                            .or_default()
                            .insert(button, handler);
                    }
                }
            }
            ElementState::Released => {
                let Some(device_buttons) = self.mouse_state.devices.get_mut(&device_id) else {
                    return;
                };
                let Some(handler) = device_buttons.remove(&button) else {
                    return;
                };
                if device_buttons.is_empty() {
                    self.mouse_state.devices.remove(&device_id);
                }

                let mut context = EventContext::new(
                    WidgetContext::new(
                        handler,
                        &self.redraw_status,
                        &self.current_theme,
                        &mut window,
                    ),
                    kludgine,
                );

                let relative = if let (Some(last_rendered), Some(location)) =
                    (context.last_layout(), self.mouse_state.location)
                {
                    Some(location - last_rendered.origin)
                } else {
                    None
                };

                context.mouse_up(relative, device_id, button);
            }
        }
    }

    fn event(
        &mut self,
        mut window: kludgine::app::Window<'_, WindowCommand>,
        _kludgine: &mut Kludgine,
        event: WindowCommand,
    ) {
        match event {
            WindowCommand::Redraw => {
                window.set_needs_redraw();
            }
        }
    }
}

fn recursively_handle_event(
    context: &mut EventContext<'_, '_>,
    mut each_widget: impl FnMut(&mut EventContext<'_, '_>) -> EventHandling,
) -> Option<ManagedWidget> {
    match each_widget(context) {
        HANDLED => Some(context.widget().clone()),
        IGNORED => context.parent().and_then(|parent| {
            recursively_handle_event(&mut context.for_other(&parent), each_widget)
        }),
    }
}

#[derive(Default)]
struct MouseState {
    location: Option<Point<Px>>,
    widget: Option<ManagedWidget>,
    devices: HashMap<DeviceId, HashMap<MouseButton, ManagedWidget>>,
}

pub(crate) mod sealed {
    use std::cell::RefCell;

    use crate::styles::ThemePair;
    use crate::value::{Dynamic, Value};
    use crate::window::WindowAttributes;

    pub struct Context<C> {
        pub user: C,
        pub settings: RefCell<WindowSettings>,
    }

    pub struct WindowSettings {
        pub attributes: Option<WindowAttributes>,
        pub occluded: Option<Dynamic<bool>>,
        pub focused: Option<Dynamic<bool>>,
        pub theme: Option<Value<ThemePair>>,
    }

    pub enum WindowCommand {
        Redraw,
        // RequestClose,
    }
}
