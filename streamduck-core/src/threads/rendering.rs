//! Rendering thread
//!
//! A separate thread for processing and rendering images on streamdeck

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use serde::{Serialize, Deserialize};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::sync::mpsc::{channel, Sender};
use std::thread::{spawn};
use image::{DynamicImage, Rgba, RgbaImage};
use image::imageops::{tile};
use rusttype::Scale;
use crate::core::{SDCore};
use crate::core::button::{Component, parse_unique_button_to_component};
use crate::core::methods::{CoreHandle, get_current_screen};
use crate::font::get_font_from_collection;
use crate::threads::streamdeck::StreamDeckCommand;
use crate::util::rendering::{image_from_horiz_gradient, image_from_solid, image_from_vert_gradient, load_image, render_aligned_shadowed_text_on_image, render_aligned_text_on_image, TextAlignment};

/// Handle for contacting renderer thread
#[derive(Debug)]
pub struct RendererHandle {
    tx: Sender<RendererCommunication>,
}

impl RendererHandle {
    /// Asks rendering thread to redraw current screen
    pub fn redraw(&self) {
        self.tx.send(RendererCommunication::Redraw).ok();
    }
}

#[allow(dead_code)]
enum RendererCommunication {
    Nothing,
    Redraw,
}

pub struct RendererState {
    render_cache: RwLock<HashMap<u64, DynamicImage>>,
    image_cache: RwLock<HashMap<u64, DynamicImage>>
}

/// Spawns rendering thread from a core reference
pub fn spawn_rendering_thread(core: Arc<SDCore>) -> RendererHandle {
    let (tx, rx) = channel::<RendererCommunication>();



    spawn(move || {
        let core = core.clone();
        let state = RendererState {
            render_cache: Default::default(),
            image_cache: Default::default()
        };

        let mut pattern = RgbaImage::new(16, 16);

        for x in 0..16 {
            for y in 0..16 {
                let color = if y < 8 {
                    if x < 8 {
                        Rgba([255, 0, 255, 255])
                    } else {
                        Rgba([0, 0, 0, 255])
                    }
                } else {
                    if x >= 8 {
                        Rgba([255, 0, 255, 255])
                    } else {
                        Rgba([0, 0, 0, 255])
                    }
                };

                pattern.put_pixel(x, y, color);
            }
        }

        let (iw, ih) = core.image_size;
        let mut frame = RgbaImage::new(iw as u32, ih as u32);

        tile(&mut frame, &pattern);

        let mut missing = DynamicImage::ImageRgba8(frame);

        if let Some(font) = get_font_from_collection("SourceHanSans-Bold.ttf") {
            render_aligned_shadowed_text_on_image(
                (iw, ih),
                &mut missing,
                &font,
                "ГДЕ",
                Scale { x: 30.0, y: 30.0 },
                TextAlignment::Center,
                0,
                (0.0, -13.0),
                (255, 0, 255, 255),
                (2, 2),
                (0, 0, 0, 255)
            );

            render_aligned_shadowed_text_on_image(
                (iw, ih),
                &mut missing,
                &font,
                "Where",
                Scale { x: 25.0, y: 25.0 },
                TextAlignment::Center,
                0,
                (0.0, 8.0),
                (255, 0, 255, 255),
                (1, 1),
                (0, 0, 0, 255)
            );
        }

        loop {
            if core.is_closed() {
                break;
            }

            if let Ok(com) = rx.recv() {
                match com {
                    RendererCommunication::Redraw => redraw(core.clone(), &state, &missing),
                    _ => {}
                }
            } else {
                break;
            }
        }

        log::trace!("rendering closed");
    });

    tx.send(RendererCommunication::Redraw).ok();

    RendererHandle {
        tx,
    }
}

fn redraw(core: Arc<SDCore>, state: &RendererState, missing: &DynamicImage) {
    let core_handle = CoreHandle::wrap(core.clone());
    let current_screen = get_current_screen(&core_handle);
    let mut commands = vec![];

    for i in 0..core.key_count {
        if let Some(current_screen) = &current_screen {
            if let Some(button) = current_screen.get(&i) {
                if let Ok(renderer) = parse_unique_button_to_component::<RendererComponent>(button) {
                    let renderer_hash = hash_renderer(&renderer);

                    let mut cache_handle = state.render_cache.write().unwrap();

                    let cache_entry = cache_handle.get(&renderer_hash);
                    let image = if cache_entry.is_some() && renderer.to_cache {
                        cache_entry.unwrap().clone()
                    } else {
                        let mut no_image = false;

                        let mut image = match renderer.background {
                            ButtonBackground::Solid(color) => {
                                image_from_solid(core.image_size, Rgba([color.0, color.1, color.2, 255]))
                            }

                            ButtonBackground::HorizontalGradient(start, end) => {
                                image_from_horiz_gradient(core.image_size, Rgba([start.0, start.1, start.2, 255]), Rgba([end.0, end.1, end.2, 255]))
                            }

                            ButtonBackground::VerticalGradient(start, end) => {
                                image_from_vert_gradient(core.image_size, Rgba([start.0, start.1, start.2, 255]), Rgba([end.0, end.1, end.2, 255]))
                            }

                            ButtonBackground::Image(path, disable_caching) => {
                                let image_hash = hash_path(&path);

                                let mut image_cache = state.image_cache.write().unwrap();
                                let image_cache_entry = image_cache.get(&image_hash);

                                let image = if image_cache_entry.is_some() && (!disable_caching) {
                                    image_cache_entry.unwrap().clone()
                                } else {
                                    let image = if let Some(image) = load_image(core.image_size, path.deref()) {
                                        image
                                    } else {
                                        no_image = true;
                                        missing.clone()
                                    };

                                    if (!disable_caching) && (!no_image) {
                                        image_cache.insert(image_hash, image.clone());
                                    }

                                    image
                                };

                                drop(image_cache);

                                image
                            }
                        };

                        for button_text in renderer.text {
                            let text = button_text.text.as_str();
                            let scale = Scale { x: button_text.scale.0, y: button_text.scale.1 };
                            let align = button_text.alignment.clone();
                            let padding = button_text.padding;
                            let offset = button_text.offset.clone();
                            let color = button_text.color.clone();

                            if let Some(font) = get_font_from_collection(&button_text.font) {
                                if let Some(shadow) = &button_text.shadow {
                                    render_aligned_shadowed_text_on_image(
                                        core.image_size,
                                        &mut image,
                                        font.as_ref(),
                                        text,
                                        scale,
                                        align,
                                        padding,
                                        offset,
                                        color,
                                        shadow.offset.clone(),
                                        shadow.color.clone()
                                    )
                                } else {
                                    render_aligned_text_on_image(
                                        core.image_size,
                                        &mut image,
                                        font.as_ref(),
                                        text,
                                        scale,
                                        align,
                                        padding,
                                        offset,
                                        color
                                    )
                                }
                            }
                        }

                        if renderer.to_cache && (!no_image) {
                            cache_handle.insert(renderer_hash, image.clone());
                        }

                        image
                    };

                    drop(cache_handle);



                    commands.push(StreamDeckCommand::SetButtonImage(i, image));
                } else {
                    commands.push(StreamDeckCommand::ClearButtonImage(i));
                }
            } else {
                commands.push(StreamDeckCommand::ClearButtonImage(i));
            }
        } else {
            commands.push(StreamDeckCommand::ClearButtonImage(i));
        }
    }

    core.send_commands(commands);
}

/// Definition for color format
pub type Color = (u8, u8, u8, u8);

/// Button Background definition for button renderer
#[derive(Serialize, Deserialize, Debug, Clone, Hash)]
pub enum ButtonBackground {
    Solid(Color),
    HorizontalGradient(Color, Color),
    VerticalGradient(Color, Color),
    Image(PathBuf, bool),
}

impl Default for ButtonBackground {
    fn default() -> Self {
        Self::Solid((0, 0, 0, 0))
    }
}

/// Button Text definition for button renderer
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ButtonText {
    pub text: String,
    pub font: String,
    pub scale: (f32, f32),
    pub alignment: TextAlignment,
    pub padding: u32,
    pub offset: (f32, f32),
    pub color: Color,
    pub shadow: Option<ButtonTextShadow>,
}

impl Hash for ButtonText {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
        self.font.hash(state);
        self.alignment.hash(state);
        self.padding.hash(state);
        self.color.hash(state);
        self.shadow.hash(state);
    }
}

/// Button text shadow
#[derive(Serialize, Deserialize, Debug, Clone, Hash)]
pub struct ButtonTextShadow {
    pub offset: (i32, i32),
    pub color: Color
}

/// Renderer component that contains button background and array of text structs
#[derive(Serialize, Deserialize, Clone, Hash, Debug)]
pub struct RendererComponent {
    #[serde(default)]
    pub background: ButtonBackground,
    #[serde(default)]
    pub text: Vec<ButtonText>,
    #[serde(default = "make_true")]
    pub to_cache: bool
}

fn make_true() -> bool { true }

impl Default for RendererComponent {
    fn default() -> Self {
        Self {
            background: ButtonBackground::Solid((255, 255, 255, 255)),
            text: vec![],
            to_cache: true
        }
    }
}

impl Component for RendererComponent {
    const NAME: &'static str = "renderer";
}

pub(crate) fn hash_renderer(renderer: &RendererComponent) -> u64 {
    let mut hasher = DefaultHasher::new();
    renderer.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn hash_path(path: &PathBuf) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}