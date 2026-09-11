use crate::storage::{self, AssetData, Content, Item, Preview, Project};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2};

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 16.0;
const MAX_INITIAL_IMAGE_SIDE: f32 = 640.0;
const PREVIEW_SIDE: u32 = 1024;
const HISTORY_LIMIT: usize = 200;

pub struct ReferenceBoardApp {
    camera: Camera,
    images: Vec<BoardImage>,
    status: String,
    tool: Tool,
    selected: Option<usize>,
    drag: Option<TransformDrag>,
    project_root: Option<PathBuf>,
    dirty: bool,
    editing: Option<usize>,
    last_save: std::time::Instant,
    pending: Option<Pending>,
    allow_close: bool,
    history: History,
    edit_before: Option<Vec<BoardImage>>,
}

#[derive(Clone, Copy)]
enum Pending {
    Open,
    Close,
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Move,
    Rotate,
    Scale,
}

struct TransformDrag {
    index: usize,
    tool: Tool,
    pointer: Pos2,
    position: Pos2,
    size: Vec2,
    rotation: f32,
    changed: bool,
}

struct Camera {
    pan: Vec2,
    zoom: f32,
}

#[derive(Clone)]
struct BoardImage {
    _path: PathBuf,
    texture: Option<TextureHandle>,
    id: String,
    content: Content,
    bytes: Option<Arc<Vec<u8>>>,
    preview: Option<Preview>,
    world_position: Pos2,
    world_size: Vec2,
    rotation: f32,
}

#[derive(Default)]
struct History {
    undo: Vec<Vec<BoardImage>>,
    redo: Vec<Vec<BoardImage>>,
}

#[derive(Clone)]
struct LoadedAsset {
    texture: TextureHandle,
}

impl History {
    fn record(&mut self, state: Vec<BoardImage>) {
        push_history(&mut self.undo, state);
        self.redo.clear();
    }

    fn undo(&mut self, current: &mut Vec<BoardImage>) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        let replaced = std::mem::replace(current, previous);
        push_history(&mut self.redo, replaced);
        true
    }

    fn redo(&mut self, current: &mut Vec<BoardImage>) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let replaced = std::mem::replace(current, next);
        push_history(&mut self.undo, replaced);
        true
    }
}

fn push_history(history: &mut Vec<Vec<BoardImage>>, state: Vec<BoardImage>) {
    if history.len() == HISTORY_LIMIT {
        history.remove(0);
    }
    history.push(state);
}

impl ReferenceBoardApp {
    fn add_note(&mut self, center: Pos2) {
        self.history.record(self.images.clone());
        self.images.push(BoardImage {
            _path: PathBuf::new(),
            texture: None,
            bytes: None,
            preview: None,
            id: new_id(),
            content: Content::Note {
                text: String::new(),
                font_size: 18.0,
                text_color: [238, 238, 238, 255],
                background: [48, 48, 48, 255],
            },
            world_position: center - Vec2::new(150.0, 80.0),
            world_size: Vec2::new(300.0, 160.0),
            rotation: 0.0,
        });
        self.selected = Some(self.images.len() - 1);
        self.editing = self.selected;
        self.edit_before = None;
        self.dirty = true;
        self.drag = None;
    }

    fn undo(&mut self) {
        if self.history.undo(&mut self.images) {
            self.selected = None;
            self.drag = None;
            self.dirty = true;
            self.status = "Undo".into();
        }
    }

    fn redo(&mut self) {
        if self.history.redo(&mut self.images) {
            self.selected = None;
            self.drag = None;
            self.dirty = true;
            self.status = "Redo".into();
        }
    }

    fn snapshot(&self) -> Project {
        Project {
            version: 1,
            camera_center: [
                -self.camera.pan.x / self.camera.zoom,
                -self.camera.pan.y / self.camera.zoom,
            ],
            zoom: self.camera.zoom,
            items: self
                .images
                .iter()
                .map(|item| Item {
                    id: item.id.clone(),
                    center: [item.center().x, item.center().y],
                    size: [item.world_size.x, item.world_size.y],
                    rotation: item.rotation,
                    content: item.content.clone(),
                })
                .collect(),
        }
    }

    fn save_project(&mut self) -> bool {
        let root = if let Some(root) = &self.project_root {
            root.clone()
        } else {
            let Some(root) = rfd::FileDialog::new()
                .set_title("Choose an empty project folder")
                .pick_folder()
            else {
                return false;
            };
            if root.join("project.json").exists() {
                self.status =
                    "This folder already contains a project. Open it or choose a new folder."
                        .into();
                return false;
            }
            root
        };
        let mut seen = std::collections::HashSet::new();
        let assets = self
            .images
            .iter()
            .filter_map(|item| match (&item.content, &item.bytes, &item.preview) {
                (Content::Image { asset, .. }, Some(bytes), Some(preview))
                    if seen.insert(asset.clone()) =>
                {
                    Some(AssetData {
                        id: asset.clone(),
                        bytes: bytes.clone(),
                        preview: preview.clone(),
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        self.last_save = std::time::Instant::now();
        match storage::save(&root, &self.snapshot(), &assets) {
            Ok(()) => {
                self.project_root = Some(root);
                for item in &mut self.images {
                    item.bytes = None;
                    item.preview = None;
                }
                self.dirty = false;
                self.status = "Project saved".into();
                true
            }
            Err(error) => {
                self.status = format!("Save failed: {error}");
                false
            }
        }
    }

    fn open_project(&mut self, ctx: &egui::Context) {
        let Some(root) = rfd::FileDialog::new()
            .set_title("Choose a project folder")
            .pick_folder()
        else {
            return;
        };
        let load_started = std::time::Instant::now();
        let path = root.join("project.json");
        let result = (|| -> storage::Result<_> {
            let project = storage::load(&path)?;
            let mut items = Vec::new();
            let mut loaded: std::collections::HashMap<String, LoadedAsset> =
                std::collections::HashMap::new();
            for data in &project.items {
                let mut item = match &data.content {
                    Content::Image { asset, .. } => {
                        let asset_path = root.join("assets").join(asset);
                        if let Some(cached) = loaded.get(asset) {
                            BoardImage {
                                _path: asset_path,
                                texture: Some(cached.texture.clone()),
                                bytes: None,
                                preview: None,
                                id: data.id.clone(),
                                content: data.content.clone(),
                                world_position: Pos2::ZERO,
                                world_size: Vec2::ZERO,
                                rotation: 0.0,
                            }
                        } else {
                            let image = BoardImage::load_asset(ctx, &root, asset)
                                .map_err(std::io::Error::other)?;
                            loaded.insert(
                                asset.clone(),
                                LoadedAsset {
                                    texture: image.texture.as_ref().unwrap().clone(),
                                },
                            );
                            image
                        }
                    }
                    Content::Note { .. } => BoardImage {
                        _path: PathBuf::new(),
                        texture: None,
                        bytes: None,
                        preview: None,
                        id: data.id.clone(),
                        content: data.content.clone(),
                        world_position: Pos2::ZERO,
                        world_size: Vec2::ZERO,
                        rotation: 0.0,
                    },
                };
                item.id = data.id.clone();
                item.content = data.content.clone();
                item.world_size = Vec2::from(data.size);
                item.world_position = Pos2::from(data.center) - item.world_size * 0.5;
                item.rotation = data.rotation;
                items.push(item);
            }
            Ok((project, items))
        })();
        match result {
            Ok((project, items)) => {
                self.images = items;
                self.camera = Camera {
                    pan: -Vec2::from(project.camera_center) * project.zoom,
                    zoom: project.zoom,
                };
                self.project_root = Some(root);
                self.dirty = false;
                self.selected = None;
                self.editing = None;
                self.drag = None;
                self.history = History::default();
                self.edit_before = None;
                self.status = format!(
                    "Project loaded in {} ms",
                    load_started.elapsed().as_millis()
                );
            }
            Err(error) => self.status = format!("Open failed (current board kept): {error}"),
        }
    }

    fn project_ui(&mut self, ctx: &egui::Context) {
        let history_shortcuts_enabled = self.editing.is_none()
            && self.drag.is_none()
            && self.pending.is_none()
            && !ctx.egui_wants_keyboard_input();
        if history_shortcuts_enabled {
            let undo = ctx.input(|i| {
                i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z)
            });
            let redo = ctx.input(|i| {
                i.modifiers.command
                    && ((i.modifiers.shift && i.key_pressed(egui::Key::Z))
                        || i.key_pressed(egui::Key::Y))
            });
            if undo {
                self.undo();
            } else if redo {
                self.redo();
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
            self.save_project();
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.pending = Some(Pending::Open);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(Pending::Close);
        }
        if let Some(index) = self.editing {
            let mut open = true;
            let mut done = false;
            let mut changed = false;
            egui::Window::new("Edit Note")
                .id(egui::Id::new("note_editor"))
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| {
                    let item = &mut self.images[index];
                    if let Content::Note {
                        text,
                        font_size,
                        text_color,
                        background,
                    } = &mut item.content
                    {
                        changed |= ui
                            .add(
                                egui::TextEdit::multiline(text)
                                    .desired_rows(8)
                                    .desired_width(f32::INFINITY),
                            )
                            .changed();
                        ui.horizontal(|ui| {
                            ui.label("Font size");
                            changed |= ui
                                .add(egui::DragValue::new(font_size).range(1.0..=10000.0))
                                .changed();
                        });
                        for (label, value) in [("Text", text_color), ("Background", background)] {
                            ui.horizontal(|ui| {
                                ui.label(label);
                                changed |= ui.color_edit_button_srgba_unmultiplied(value).changed();
                            });
                        }
                        ui.horizontal(|ui| {
                            ui.label("Width / Height");
                            let center = item.world_position + item.world_size * 0.5;
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut item.world_size.x)
                                        .range(24.0..=100000.0),
                                )
                                .changed();
                            changed |= ui
                                .add(
                                    egui::DragValue::new(&mut item.world_size.y)
                                        .range(24.0..=100000.0),
                                )
                                .changed();
                            item.world_position = center - item.world_size * 0.5;
                        });
                        done = ui.button("Done").clicked();
                    }
                });
            if changed {
                self.dirty = true;
                if let Some(before) = self.edit_before.take() {
                    self.history.record(before);
                }
            }
            if !open || done {
                self.editing = None;
                self.edit_before = None;
            }
        }
        if let Some(action) = self.pending {
            let mut proceed = !self.dirty;
            if self.dirty {
                egui::Window::new("Unsaved changes")
                    .collapsible(false)
                    .resizable(false)
                    .show(ctx, |ui| {
                        ui.label("Save this board before continuing?");
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                proceed = self.save_project();
                            }
                            if ui.button("Discard changes").clicked() {
                                proceed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                self.pending = None;
                            }
                        });
                    });
            }
            if proceed {
                self.pending = None;
                match action {
                    Pending::Open => self.open_project(ctx),
                    Pending::Close => {
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
        if self.dirty
            && self.project_root.is_some()
            && self.drag.is_none()
            && self.editing.is_none()
            && self.pending.is_none()
            && self.last_save.elapsed().as_secs() >= 3
        {
            self.save_project();
        }
        if self.dirty && self.project_root.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs(3));
        }
    }

    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        creation_context.egui_ctx.set_visuals(egui::Visuals::dark());
        let candidates = [
            "C:/Windows/Fonts/malgun.ttf",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        ];
        for path in candidates {
            if let Ok(bytes) = std::fs::read(path) {
                let mut fonts = egui::FontDefinitions::default();
                fonts
                    .font_data
                    .insert("korean".into(), egui::FontData::from_owned(bytes).into());
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .push("korean".into());
                }
                creation_context.egui_ctx.set_fonts(fonts);
                break;
            }
        }

        Self {
            camera: Camera::default(),
            images: Vec::new(),
            status: "Drop image files onto the canvas".to_owned(),
            tool: Tool::Move,
            selected: None,
            drag: None,
            project_root: None,
            dirty: false,
            editing: None,
            last_save: std::time::Instant::now(),
            pending: None,
            allow_close: false,
            history: History::default(),
            edit_before: None,
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        let (paths, drop_position) = ctx.input(|input| {
            let paths = input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect::<Vec<_>>();
            (paths, input.pointer.hover_pos())
        });

        if paths.is_empty() {
            return;
        }

        let screen_position = drop_position.unwrap_or(canvas_rect.center());
        let first_world_position = self.camera.screen_to_world(screen_position, canvas_rect);
        let mut loaded = 0;
        let mut failures = Vec::new();
        let mut added = Vec::new();

        for (index, path) in paths.into_iter().enumerate() {
            let cascade = Vec2::splat(index as f32 * 24.0);
            match BoardImage::load(ctx, &path, first_world_position + cascade) {
                Ok(image) => {
                    added.push(image);
                    loaded += 1;
                }
                Err(error) => failures.push(error),
            }
        }

        if !added.is_empty() {
            self.history.record(self.images.clone());
            self.images.extend(added);
            self.dirty = true;
            self.selected = Some(self.images.len() - 1);
        }

        self.status = match (loaded, failures.is_empty()) {
            (0, _) => failures.join(" | "),
            (_, true) => format!("Added {loaded} image(s)"),
            (_, false) => format!("Added {loaded}; some failed: {}", failures.join(" | ")),
        };
    }

    fn draw_grid(&self, painter: &egui::Painter, canvas_rect: Rect) {
        let grid_step = visible_grid_step(self.camera.zoom);
        let origin = self.camera.world_to_screen(Pos2::ZERO, canvas_rect);
        let stroke = Stroke::new(1.0, Color32::from_gray(48));

        let mut x = origin.x.rem_euclid(grid_step) + canvas_rect.left();
        while x <= canvas_rect.right() {
            painter.line_segment(
                [
                    Pos2::new(x, canvas_rect.top()),
                    Pos2::new(x, canvas_rect.bottom()),
                ],
                stroke,
            );
            x += grid_step;
        }

        let mut y = origin.y.rem_euclid(grid_step) + canvas_rect.top();
        while y <= canvas_rect.bottom() {
            painter.line_segment(
                [
                    Pos2::new(canvas_rect.left(), y),
                    Pos2::new(canvas_rect.right(), y),
                ],
                stroke,
            );
            y += grid_step;
        }
    }

    fn draw_images(&self, painter: &egui::Painter, canvas_rect: Rect) {
        for (index, image) in self.images.iter().enumerate() {
            let corners = image
                .corners()
                .map(|p| self.camera.world_to_screen(p, canvas_rect));
            if canvas_rect.intersects(Rect::from_points(&corners)) {
                if let Some(texture) = &image.texture {
                    let mut mesh = egui::Mesh::with_texture(texture.id());
                    let uvs = [
                        Pos2::new(0.0, 0.0),
                        Pos2::new(1.0, 0.0),
                        Pos2::new(1.0, 1.0),
                        Pos2::new(0.0, 1.0),
                    ];
                    for (pos, uv) in corners.into_iter().zip(uvs) {
                        mesh.vertices.push(egui::epaint::Vertex {
                            pos,
                            uv,
                            color: Color32::WHITE,
                        });
                    }
                    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
                    painter.add(mesh);
                } else if let Content::Note {
                    text,
                    font_size,
                    text_color,
                    background,
                } = &image.content
                {
                    painter.add(egui::Shape::convex_polygon(
                        corners.to_vec(),
                        Color32::from_rgba_unmultiplied(
                            background[0],
                            background[1],
                            background[2],
                            background[3],
                        ),
                        Stroke::NONE,
                    ));
                    let color = Color32::from_rgba_unmultiplied(
                        text_color[0],
                        text_color[1],
                        text_color[2],
                        text_color[3],
                    );
                    let padding = 8.0 * self.camera.zoom;
                    let mut job = egui::text::LayoutJob::simple(
                        text.clone(),
                        egui::FontId::proportional(font_size * self.camera.zoom),
                        color,
                        (image.world_size.x * self.camera.zoom - padding * 2.0).max(1.0),
                    );
                    job.wrap.max_rows = ((image.world_size.y * self.camera.zoom - padding * 2.0)
                        / (font_size * self.camera.zoom * 1.2))
                        .max(1.0) as usize;
                    let galley = painter.layout_job(job);
                    let pos = corners[0] + rotate(Vec2::splat(padding), image.rotation);
                    let mut text_shape = egui::epaint::TextShape::new(pos, galley, color);
                    text_shape.angle = image.rotation;
                    painter.add(text_shape);
                }
                if self.selected == Some(index) {
                    painter.add(egui::Shape::closed_line(
                        corners.to_vec(),
                        Stroke::new(2.0, Color32::LIGHT_BLUE),
                    ));
                    painter.circle_filled(
                        self.camera.world_to_screen(image.center(), canvas_rect),
                        3.0,
                        Color32::LIGHT_BLUE,
                    );
                }
            }
        }
    }

    fn transform_input(&mut self, ui: &egui::Ui, response: &egui::Response, canvas: Rect) {
        if self.editing.is_some() || self.pending.is_some() {
            self.drag = None;
            return;
        }
        if response.double_clicked() {
            let pointer = response.interact_pointer_pos().unwrap_or(canvas.center());
            if let Some(index) = self
                .images
                .iter()
                .rposition(|item| item.contains(self.camera.screen_to_world(pointer, canvas)))
                && matches!(self.images[index].content, Content::Note { .. })
            {
                self.edit_before = Some(self.images.clone());
                self.editing = Some(index);
                self.drag = None;
                return;
            }
        }
        if !ui.ctx().egui_wants_keyboard_input() && self.drag.is_none() {
            for (key, tool) in [
                (egui::Key::Q, Tool::Move),
                (egui::Key::W, Tool::Rotate),
                (egui::Key::E, Tool::Scale),
            ] {
                if ui.input(|i| i.modifiers.is_none() && i.key_pressed(key)) {
                    self.tool = tool;
                }
            }
        }
        let (pressed, down, pointer, focused) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.interact_pos(),
                i.focused,
            )
        });
        if !down || !focused {
            self.drag = None;
            return;
        }
        let Some(pointer) = pointer else { return };
        if pressed && response.contains_pointer() {
            let world = self.camera.screen_to_world(pointer, canvas);
            self.selected = self.images.iter().rposition(|image| image.contains(world));
            self.drag = self.selected.map(|index| {
                let image = &self.images[index];
                TransformDrag {
                    index,
                    tool: self.tool,
                    pointer,
                    position: image.world_position,
                    size: image.world_size,
                    rotation: image.rotation,
                    changed: false,
                }
            });
        }
        let should_record = self
            .drag
            .as_ref()
            .is_some_and(|drag| !drag.changed && pointer != drag.pointer);
        if should_record {
            self.history.record(self.images.clone());
            self.drag.as_mut().unwrap().changed = true;
        }
        if let Some(drag) = &self.drag {
            let image = &mut self.images[drag.index];
            if pointer != drag.pointer {
                self.dirty = true;
            }
            drag.apply(image, pointer, self.camera.zoom);
        }
    }
}

impl TransformDrag {
    fn apply(&self, image: &mut BoardImage, pointer: Pos2, zoom: f32) {
        let delta = pointer - self.pointer;
        match self.tool {
            Tool::Move => image.world_position = self.position + delta / zoom,
            Tool::Rotate => image.rotation = self.rotation + delta.x * 0.01,
            Tool::Scale => {
                let factor = (delta.x * 0.005).clamp(-4.0, 4.0).exp();
                let previous_width = image.world_size.x;
                image.world_size = self.size * factor;
                if let Content::Note { font_size, .. } = &mut image.content {
                    *font_size *= image.world_size.x / previous_width;
                }
                image.world_position = self.position + (self.size - image.world_size) * 0.5;
            }
        }
    }
}

impl eframe::App for ReferenceBoardApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let canvas_rect = ui.max_rect();
        let response = ui.allocate_rect(canvas_rect, Sense::click_and_drag());
        response.context_menu(|ui| {
            if ui.button("Add Note").clicked() {
                self.add_note(
                    self.camera.screen_to_world(
                        response
                            .interact_pointer_pos()
                            .unwrap_or(canvas_rect.center()),
                        canvas_rect,
                    ),
                );
                ui.close();
            }
        });
        self.transform_input(ui, &response, canvas_rect);

        if self.drag.is_none()
            && (response.dragged_by(egui::PointerButton::Middle)
                || response.dragged_by(egui::PointerButton::Secondary))
        {
            self.camera.pan += ui.input(|input| input.pointer.delta());
            self.dirty = true;
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }

        if response.hovered() && self.drag.is_none() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll.abs() > f32::EPSILON {
                let pointer = ui
                    .input(|input| input.pointer.hover_pos())
                    .unwrap_or(canvas_rect.center());
                let zoom_factor = (scroll * 0.002).exp();
                self.camera.zoom_at(pointer, zoom_factor, canvas_rect);
                self.dirty = true;
            }
        }

        if self.editing.is_none()
            && self.drag.is_none()
            && ui.input(|input| input.key_pressed(egui::Key::Num0))
        {
            self.camera = Camera::default();
            self.dirty = true;
        }

        self.handle_dropped_files(ui.ctx(), canvas_rect);

        let painter = ui.painter_at(canvas_rect);
        painter.rect_filled(canvas_rect, 0.0, Color32::from_rgb(28, 29, 32));
        self.draw_grid(&painter, canvas_rect);
        self.draw_images(&painter, canvas_rect);

        egui::Area::new(egui::Id::new("status"))
            .fixed_pos(canvas_rect.left_top() + Vec2::splat(12.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(180))
                    .corner_radius(6.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.label(&self.status);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                self.save_project();
                            }
                            if ui.button("Open").clicked() {
                                self.pending = Some(Pending::Open);
                            }
                            if ui.button("Add Note").clicked() {
                                self.add_note(
                                    self.camera
                                        .screen_to_world(canvas_rect.center(), canvas_rect),
                                );
                            }
                            if ui
                                .add_enabled(
                                    !self.history.undo.is_empty() && self.editing.is_none(),
                                    egui::Button::new("Undo"),
                                )
                                .clicked()
                            {
                                self.undo();
                            }
                            if ui
                                .add_enabled(
                                    !self.history.redo.is_empty() && self.editing.is_none(),
                                    egui::Button::new("Redo"),
                                )
                                .clicked()
                            {
                                self.redo();
                            }
                            if self.dirty {
                                ui.label("Unsaved");
                            }
                        });
                        ui.add_enabled_ui(self.drag.is_none(), |ui| {
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.tool, Tool::Move, "Q Move");
                                ui.selectable_value(&mut self.tool, Tool::Rotate, "W Rotate");
                                ui.selectable_value(&mut self.tool, Tool::Scale, "E Scale");
                            });
                        });
                        ui.small("Left drag: edit | Rotate / Scale: drag left or right");
                        ui.small(format!(
                            "Zoom {:.0}% · {} image(s)",
                            self.camera.zoom * 100.0,
                            self.images.len()
                        ));
                    });
            });
        self.project_ui(ui.ctx());
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
        }
    }
}

impl Camera {
    fn world_to_screen(&self, world: Pos2, canvas_rect: Rect) -> Pos2 {
        canvas_rect.center() + self.pan + world.to_vec2() * self.zoom
    }

    fn screen_to_world(&self, screen: Pos2, canvas_rect: Rect) -> Pos2 {
        ((screen - canvas_rect.center() - self.pan) / self.zoom).to_pos2()
    }

    fn zoom_at(&mut self, screen_position: Pos2, factor: f32, canvas_rect: Rect) {
        let world_position = self.screen_to_world(screen_position, canvas_rect);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = screen_position - canvas_rect.center() - world_position.to_vec2() * self.zoom;
    }
}

impl BoardImage {
    fn center(&self) -> Pos2 {
        self.world_position + self.world_size * 0.5
    }

    fn corners(&self) -> [Pos2; 4] {
        let half = self.world_size * 0.5;
        [
            Vec2::new(-half.x, -half.y),
            Vec2::new(half.x, -half.y),
            half,
            Vec2::new(-half.x, half.y),
        ]
        .map(|offset| self.center() + rotate(offset, self.rotation))
    }

    fn contains(&self, world: Pos2) -> bool {
        let local = rotate(world - self.center(), -self.rotation);
        local.x.abs() <= self.world_size.x * 0.5 && local.y.abs() <= self.world_size.y * 0.5
    }

    fn load(ctx: &egui::Context, path: &Path, world_position: Pos2) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let asset = storage::asset_id(&bytes);
        let (preview, pixel_size) = decode_preview(ctx, &bytes, path)?;
        let texture = texture_from_preview(ctx, &path.to_string_lossy(), &preview);

        Ok(Self {
            _path: path.to_path_buf(),
            texture: Some(texture),
            id: new_id(),
            content: Content::Image {
                asset,
                original_name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            },
            bytes: Some(Arc::new(bytes)),
            preview: Some(preview),
            world_position,
            world_size: fit_initial_size(pixel_size),
            rotation: 0.0,
        })
    }

    fn load_asset(ctx: &egui::Context, root: &Path, asset: &str) -> Result<Self, String> {
        let path = root.join("assets").join(asset);
        if !path.is_file() {
            return Err(format!("{}: missing asset", path.display()));
        }
        let preview = match storage::load_preview(root, asset) {
            Ok(preview) => {
                let needs_resize = preview.width > PREVIEW_SIDE || preview.height > PREVIEW_SIDE;
                let smaller = constrain_preview(preview);
                if needs_resize {
                    storage::save_preview(root, asset, &smaller)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                }
                smaller
            }
            Err(_) => {
                let bytes =
                    std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                if storage::asset_id(&bytes) != asset {
                    return Err(format!("{}: asset hash mismatch", path.display()));
                }
                let (preview, _) = decode_preview(ctx, &bytes, &path)?;
                storage::save_preview(root, asset, &preview)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                preview
            }
        };
        let texture = texture_from_preview(ctx, asset, &preview);

        Ok(Self {
            _path: path,
            texture: Some(texture),
            id: new_id(),
            content: Content::Image {
                asset: asset.to_owned(),
                original_name: asset.to_owned(),
            },
            bytes: None,
            preview: None,
            world_position: Pos2::ZERO,
            world_size: Vec2::ZERO,
            rotation: 0.0,
        })
    }
}

fn decode_preview(
    ctx: &egui::Context,
    bytes: &[u8],
    path: &Path,
) -> Result<(Preview, Vec2), String> {
    let decoded = image::load_from_memory(bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .into_rgba8();
    let pixel_size = Vec2::new(decoded.width() as f32, decoded.height() as f32);
    let limit = ctx.input(|i| i.max_texture_side).min(PREVIEW_SIDE as usize) as u32;
    let preview = image::DynamicImage::ImageRgba8(decoded)
        .thumbnail(limit, limit)
        .into_rgba8();
    Ok((
        Preview {
            width: preview.width(),
            height: preview.height(),
            rgba: Arc::new(preview.into_raw()),
        },
        pixel_size,
    ))
}

fn constrain_preview(preview: Preview) -> Preview {
    if preview.width <= PREVIEW_SIDE && preview.height <= PREVIEW_SIDE {
        return preview;
    }
    let source =
        image::RgbaImage::from_raw(preview.width, preview.height, preview.rgba.as_ref().clone())
            .expect("validated preview dimensions");
    let smaller = image::DynamicImage::ImageRgba8(source)
        .thumbnail(PREVIEW_SIDE, PREVIEW_SIDE)
        .into_rgba8();
    Preview {
        width: smaller.width(),
        height: smaller.height(),
        rgba: Arc::new(smaller.into_raw()),
    }
}

fn texture_from_preview(ctx: &egui::Context, name: &str, preview: &Preview) -> TextureHandle {
    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [preview.width as usize, preview.height as usize],
        &preview.rgba,
    );
    ctx.load_texture(name, color_image, TextureOptions::LINEAR)
}

fn rotate(vector: Vec2, angle: f32) -> Vec2 {
    let (sin, cos) = angle.sin_cos();
    Vec2::new(
        cos * vector.x - sin * vector.y,
        sin * vector.x + cos * vector.y,
    )
}

fn new_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{:x}-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn fit_initial_size(pixel_size: Vec2) -> Vec2 {
    let longest_side = pixel_size.x.max(pixel_size.y);
    if longest_side <= MAX_INITIAL_IMAGE_SIDE {
        pixel_size
    } else {
        pixel_size * (MAX_INITIAL_IMAGE_SIDE / longest_side)
    }
}

fn visible_grid_step(zoom: f32) -> f32 {
    let mut step = 100.0 * zoom;
    while step < 32.0 {
        step *= 2.0;
    }
    while step > 160.0 {
        step *= 0.5;
    }
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image() -> BoardImage {
        let ctx = egui::Context::default();
        BoardImage {
            _path: PathBuf::new(),
            texture: Some(ctx.load_texture(
                "test",
                egui::ColorImage::new([1, 1], vec![Color32::WHITE]),
                TextureOptions::LINEAR,
            )),
            id: "test".into(),
            content: Content::Image {
                asset: "a".repeat(64),
                original_name: "test.png".into(),
            },
            bytes: None,
            preview: None,
            world_position: Pos2::new(10.0, 20.0),
            world_size: Vec2::new(200.0, 100.0),
            rotation: std::f32::consts::FRAC_PI_2,
        }
    }

    #[test]
    fn rotated_image_hit_test_matches_visible_rectangle() {
        let image = sample_image();
        assert!(image.contains(image.center() + Vec2::new(0.0, 90.0)));
        assert!(!image.contains(image.center() + Vec2::new(90.0, 0.0)));
    }

    #[test]
    fn history_restores_state_and_new_change_clears_redo() {
        let mut history = History::default();
        let mut images = Vec::new();
        history.record(images.clone());
        images.push(sample_image());

        assert!(history.undo(&mut images));
        assert!(images.is_empty());
        assert!(history.redo(&mut images));
        assert_eq!(images.len(), 1);

        assert!(history.undo(&mut images));
        history.record(images.clone());
        images.push(sample_image());
        assert!(!history.redo(&mut images));
    }

    #[test]
    fn transforms_preserve_center_and_scale_move_with_camera_zoom() {
        let mut image = sample_image();
        let center = image.center();
        let mut drag = TransformDrag {
            index: 0,
            tool: Tool::Scale,
            pointer: Pos2::ZERO,
            position: image.world_position,
            size: image.world_size,
            rotation: image.rotation,
            changed: false,
        };
        drag.apply(&mut image, Pos2::new(100.0, 0.0), 2.0);
        assert!(image.center().distance(center) < 0.001);
        assert!((image.world_size.x / image.world_size.y - 2.0).abs() < 0.001);
        drag.tool = Tool::Rotate;
        drag.apply(&mut image, Pos2::new(100.0, 0.0), 2.0);
        assert!((image.rotation - drag.rotation - 1.0).abs() < 0.001);
        assert!(image.center().distance(center) < 0.001);
        drag.tool = Tool::Move;
        drag.apply(&mut image, Pos2::new(100.0, 40.0), 2.0);
        assert_eq!(image.world_position, drag.position + Vec2::new(50.0, 20.0));
    }

    #[test]
    fn camera_transform_round_trips() {
        let camera = Camera {
            pan: Vec2::new(120.0, -80.0),
            zoom: 2.5,
        };
        let canvas = Rect::from_min_size(Pos2::new(20.0, 30.0), Vec2::new(800.0, 600.0));
        let world = Pos2::new(42.0, -17.0);

        let restored = camera.screen_to_world(camera.world_to_screen(world, canvas), canvas);

        assert!((restored.x - world.x).abs() < 0.001);
        assert!((restored.y - world.y).abs() < 0.001);
    }

    #[test]
    fn initial_image_size_preserves_aspect_ratio() {
        let fitted = fit_initial_size(Vec2::new(4000.0, 2000.0));

        assert_eq!(fitted, Vec2::new(640.0, 320.0));
    }
}
