use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::Arc;
use std::time::Instant;

use image::{DynamicImage, Rgba};
use serde::de::Error as DeError;
use serde_json::{Map, Value};
use serde_json::Error as JSONError;
use tokio::sync::MutexGuard;

use crate::{Config, ModuleManager, SDCore, SocketManager};
use crate::core::{ButtonPanel, UniqueButton};
use crate::core::button::{Button, parse_unique_button_to_component};
use crate::modules::{features_to_vec, UniqueSDModule};
use crate::modules::components::{UIPathValue, UIValue};
use crate::modules::core_module::CoreSettings;
use crate::modules::events::SDCoreEvent;
use crate::thread::DeviceThreadCommunication;
use crate::thread::rendering::{draw_background, draw_custom_renderer_texture, draw_foreground, draw_missing_texture, RendererComponent};
use crate::thread::util::image_from_solid;
use crate::util::{add_array_function, button_to_raw, change_from_path, convert_value_to_path, deserialize_panel, make_button_unique, panel_to_raw, remove_array_function, serialize_panel, set_value_function};
use crate::versions::SUPPORTED_FEATURES;

/// Handle that's given out to a module to perform actions on the core
#[derive(Clone)]
pub struct CoreHandle {
    pub(crate) core: Arc<SDCore>,
    pub(crate) module_name: String,
    pub(crate) module_features: Vec<(String, String)>,
}

/// Checks if slice of features contains a specific feature
pub fn check_feature_list_for_feature(features: &Vec<(String, String)>, feature: &str) -> bool {
    for (feat, _) in features {
        if *feat == feature {
            return true;
        }
    }

    false
}

/// Warns if slice of features doesn't contain a specific feature
pub fn warn_for_feature(module_name: &str, features: &Vec<(String, String)>, feature: &str) {
    if !check_feature_list_for_feature(features, feature) {
        log::warn!("Module '{}' is using unreported feature '{}', please add the feature into plugin metadata to prevent any future crashes due to version incompatibility", module_name, feature);
    }
}

impl CoreHandle {
    /// Wraps core reference with a handle, used for all core features to be able to bypass feature checking
    pub fn wrap(core: Arc<SDCore>) -> CoreHandle {
        CoreHandle {
            core,
            module_name: "-system-".to_string(),
            module_features: features_to_vec(SUPPORTED_FEATURES)
        }
    }

    /// Checks if module is allowed to use this feature
    pub fn check_for_feature(&self, feature: &str) -> bool {
        check_feature_list_for_feature(&self.module_features, feature)
    }

    /// Warns if module is using feature it hasn't reported
    pub fn required_feature(&self, feature: &str) {
        warn_for_feature(&self.module_name, &self.module_features, feature)
    }

    /// Clones the handle for specified module
    pub fn clone_for(&self, module: &UniqueSDModule) -> CoreHandle {
        CoreHandle {
            core: self.core.clone(),
            module_name: module.name(),
            module_features: module.metadata().used_features
        }
    }

    /// Returns core reference
    pub fn core(&self) -> Arc<SDCore> {
        self.required_feature("core");
        self.core.clone()
    }

    /// Returns config reference
    pub fn config(&self) -> Arc<Config> {
        self.required_feature("config");
        self.core.config.clone()
    }

    /// Returns module manager reference
    pub fn module_manager(&self) -> Arc<ModuleManager> {
        self.required_feature("module_manager");
        self.core.module_manager.clone()
    }

    /// Returns socket manager reference
    pub fn socket_manager(&self) -> Arc<SocketManager> {
        self.required_feature("socket_api");
        self.core.socket_manager.clone()
    }

    /// Returns current stack lock
    pub async fn current_stack(&self) -> MutexGuard<'_, Vec<ButtonPanel>> {
        self.required_feature("core");
        self.core.current_stack.lock().await
    }

    /// Sends core event to all modules, spawns a separate thread to do it, so doesn't block current thread
    pub async fn send_core_event_to_modules<T: Iterator<Item=UniqueSDModule> + Send + 'static>(&self, event: SDCoreEvent, modules: T) {
        let core = self.clone();
        for module in modules {
            if module.name() == core.module_name {
                continue;
            }

            let task_core = core.clone_for(&module);
            let task_event = event.clone();
            tokio::spawn(async move {
                module.event(task_core, task_event).await;
            });
        }
    }

    /// Gets current panel stack
    pub async fn get_stack(&self) -> Vec<ButtonPanel> {
        self.required_feature("core_methods");
        let stack = self.current_stack().await;

        stack.iter().map(|x| x.clone()).collect()
    }

    /// Gets panel that's currently on top of the stack
    pub async fn get_current_screen(&self) -> Option<ButtonPanel> {
        self.required_feature("core_methods");
        let stack = self.current_stack().await;

        if let Some(screen) = stack.last() {
            Some(screen.clone())
        } else {
            None
        }
    }

    /// Returns a button from current screen on specified position
    pub async fn get_button(&self, key: u8) -> Option<UniqueButton> {
        self.required_feature("core_methods");
        if let Some(screen) = self.get_current_screen().await {
            let handle = screen.read().await;
            handle.buttons.get(&key).cloned()
        } else {
            None
        }
    }

    /// Sets button to current screen with specified position
    pub async fn set_button(&self, key: u8, button: UniqueButton) -> bool {
        self.required_feature("core_methods");
        if let Some(screen) = self.get_current_screen().await {
            let mut handle = screen.write().await;
            let previous_button = handle.buttons.get(&key).cloned();

            handle.buttons.insert(key, button.clone());

            drop(handle);

            if let Some(previous_button) = previous_button {
                self.send_core_event_to_modules(SDCoreEvent::ButtonUpdated {
                    key,
                    panel: screen.clone(),
                    new_button: button.clone(),
                    old_button: previous_button.clone()
                }, self.module_manager().get_module_list().await.into_iter()).await;
            } else {
                self.send_core_event_to_modules( SDCoreEvent::ButtonAdded {
                    key,
                    panel: screen.clone(),
                    added_button: button.clone()
                }, self.module_manager().get_module_list().await.into_iter()).await;
            }

            self.core.mark_for_redraw().await;

            true
        } else {
            false
        }
    }

    /// Clears button from current screen on specified position
    pub async fn clear_button(&self, key: u8) -> bool {
        self.required_feature("core_methods");
        if let Some(screen) = self.get_current_screen().await {
            let mut handle = screen.write().await;
            if let Some(button) = handle.buttons.remove(&key) {
                drop(handle);

                self.send_core_event_to_modules( SDCoreEvent::ButtonDeleted {
                    key,
                    panel: screen.clone(),
                    deleted_button: button.clone()
                }, self.module_manager().get_module_list().await.into_iter()).await;

                self.core.mark_for_redraw().await;

                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Adds component onto a button, returns success boolean
    pub async fn add_component(&self, key: u8, component_name: &str) -> bool {
        self.required_feature("core_methods");

        let module_manager = self.module_manager();

        if let Some(screen) = self.get_current_screen().await {
            let handle = screen.read().await;
            if let Some(button) = handle.buttons.get(&key).cloned() {
                let previous = make_button_unique(button_to_raw(&button).await);

                let mut button_handle = button.write().await;
                drop(handle);

                if !button_handle.component_names().contains(&component_name.to_string()) {
                    let components = module_manager.read_component_map().await;

                    if let Some((_, module)) = components.get(component_name) {
                        module.add_component(self.clone_for(&module), button_handle.deref_mut(), component_name).await;

                        drop(button_handle);
                        drop(components);

                        self.send_core_event_to_modules(SDCoreEvent::ButtonUpdated {
                            key,
                            panel: screen.clone(),
                            new_button: button.clone(),
                            old_button: previous.clone()
                        }, self.module_manager().get_module_list().await.into_iter()).await;

                        self.core.mark_for_redraw().await;

                        return true;
                    }
                }
            }
        }

        false
    }

    /// Gets component values from a component on a button
    pub async fn get_component_values(&self, key: u8, component_name: &str) -> Option<Vec<UIValue>> {
        self.required_feature("core_methods");

        let module_manager = self.module_manager();

        if let Some(screen) = self.get_current_screen().await {
            let handle = screen.read().await;
            if let Some(button) = handle.buttons.get(&key).cloned() {
                let mut button_handle = button.write().await;
                drop(handle);

                if button_handle.component_names().contains(&component_name.to_string()) {
                    let components = module_manager.read_component_map().await;

                    if let Some((_, module)) = components.get(component_name) {
                        return Some(module.component_values(self.clone_for(&module), button_handle.deref_mut(), component_name).await);
                    }
                }
            }
        }

        None
    }

    /// Gets component values from component on a button, but with paths for easier interaction with values
    pub async fn get_component_values_with_paths(&self, key: u8, component_name: &str) -> Option<Vec<UIPathValue>> {
        self.required_feature("core_methods");

        if let Some(values) = self.get_component_values(key, component_name).await {
            Some(values.into_iter().map(|x| convert_value_to_path(x, "")).collect())
        } else {
            None
        }
    }

    /// Sets component values based on changes for component on a button
    pub async fn set_component_value(&self, key: u8, component_name: &str, value: Vec<UIValue>) -> bool {
        self.required_feature("core_methods");

        let module_manager = self.module_manager();

        if let Some(screen) = self.get_current_screen().await {
            let handle = screen.read().await;
            if let Some(button) = handle.buttons.get(&key).cloned() {
                let previous = make_button_unique(button_to_raw(&button).await);

                let mut button_handle = button.write().await;
                drop(handle);

                if button_handle.component_names().contains(&component_name.to_string()) {
                    let components = module_manager.read_component_map().await;

                    if let Some((_, module)) = components.get(component_name) {
                        module.set_component_value(self.clone_for(&module), button_handle.deref_mut(), component_name, value).await;
                        drop(button_handle);
                        drop(components);

                        self.send_core_event_to_modules(SDCoreEvent::ButtonUpdated {
                            key,
                            panel: screen.clone(),
                            new_button: button.clone(),
                            old_button: previous.clone()
                        }, self.module_manager().get_module_list().await.into_iter()).await;

                        self.core.mark_for_redraw().await;

                        return true;
                    }
                }
            }
        }

        false
    }

    /// Adds new array element to a component value
    pub async fn add_element_component_value(&self, key: u8, component_name: &str, path: &str) -> bool {
        self.required_feature("core_methods");

        if let Some(values) = self.get_component_values(key, component_name).await {
            let (changes, success) = change_from_path(path, values, &add_array_function(), false);

            if success {
                if !changes.is_empty() {
                    self.set_component_value(key, component_name, changes).await
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Removes element from array in component value
    pub async fn remove_element_component_value(&self, key: u8, component_name: &str, path: &str, index: usize) -> bool {
        self.required_feature("core_methods");

        if let Some(values) = self.get_component_values(key, component_name).await {
            let (changes, success) = change_from_path(path, values, &remove_array_function(index), false);

            if success {
                if !changes.is_empty() {
                    self.set_component_value(key, component_name, changes).await
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Sets value based on path for component value
    pub async fn set_component_value_by_path(&self, key: u8, component_name: &str, value: UIPathValue) -> bool {
        self.required_feature("core_methods");

        if let Some(values) = self.get_component_values(key, component_name).await {
            let (changes, success) = change_from_path(&value.path, values, &set_value_function(value.clone()), false);

            if success {
                if !changes.is_empty() {
                    self.set_component_value(key, component_name, changes).await
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Removes component from a button
    pub async fn remove_component(&self, key: u8, component_name: &str) -> bool {
        self.required_feature("core_methods");

        let module_manager = self.module_manager();

        if let Some(screen) = self.get_current_screen().await {
            let handle = screen.read().await;
            if let Some(button) = handle.buttons.get(&key).cloned() {
                let previous = make_button_unique(button_to_raw(&button).await);

                let mut button_handle = button.write().await;
                drop(handle);

                if button_handle.component_names().contains(&component_name.to_string()) {
                    let components = module_manager.read_component_map().await;

                    if let Some((_, module)) = components.get(component_name) {
                        module.remove_component(self.clone_for(&module), button_handle.deref_mut(), component_name).await;

                        drop(button_handle);
                        drop(components);

                        self.send_core_event_to_modules(SDCoreEvent::ButtonUpdated {
                            key,
                            panel: screen.clone(),
                            new_button: button.clone(),
                            old_button: previous.clone()
                        }, self.module_manager().get_module_list().await.into_iter()).await;

                        self.core.mark_for_redraw().await;

                        return true;
                    }
                }
            }
        }

        false
    }

    /// Creates a new button taking provided one as an example and makes all responsible modules handle the paste action
    pub async fn paste_button(&self, key: u8, reference_button: &Button) -> bool {
        let mut new_button = Button::new();

        let responsible_modules = self.module_manager().get_modules_for_declared_components(reference_button.component_names().as_slice()).await;
        for module in responsible_modules {
            module.paste_component(self.clone_for(&module), reference_button, &mut new_button).await;
        }

        println!("resulting button: {:?}", new_button);

        self.set_button(key, make_button_unique(new_button)).await
    }

    /// Pushes new panel into the stack
    pub async fn push_screen(&self, screen: ButtonPanel) {
        self.required_feature("core_methods");
        let mut stack = self.current_stack().await;

        stack.push(screen.clone());
        drop(stack);

        self.send_core_event_to_modules(SDCoreEvent::PanelPushed {
            new_panel: screen.clone()
        }, self.module_manager().get_module_list().await.into_iter()).await;

        self.core.mark_for_redraw().await;
    }

    /// Pops panel from stack
    pub async fn pop_screen(&self) {
        self.required_feature("core_methods");
        let mut stack = self.current_stack().await;

        let old_panel = stack.pop();
        drop(stack);

        if let Some(old_panel) = old_panel {
            self.send_core_event_to_modules(SDCoreEvent::PanelPopped {
                popped_panel: old_panel.clone()
            }, self.module_manager().get_module_list().await.into_iter()).await;
        }

        self.core.mark_for_redraw().await;
    }

    /// Returns first panel of the stack for saving purposes
    pub async fn get_root_screen(&self) -> ButtonPanel {
        self.required_feature("core_methods");
        let stack = self.current_stack().await;
        stack.get(0).unwrap().clone()
    }

    /// Returns first panel of the stack that's already been serialized
    pub async fn save_panels_to_value(&self) -> Value {
        self.required_feature("core_methods");
        let stack = self.current_stack().await;

        if let Some(panel) = stack.get(0) {
            let serialized_panel = serialize_panel(panel.clone()).await.unwrap();
            serde_json::to_value(&serialized_panel).unwrap()
        } else {
            Value::Object(Map::new())
        }
    }

    /// Clears the stack and loads provided panel into the stack
    pub async fn reset_stack(&self, panel: ButtonPanel) {
        self.required_feature("core_methods");
        let mut stack = self.current_stack().await;

        stack.clear();
        stack.push(panel.clone());
        drop(stack);

        self.send_core_event_to_modules(SDCoreEvent::StackReset {
            new_panel: panel.clone()
        }, self.module_manager().get_module_list().await.into_iter()).await;

        self.core.mark_for_redraw().await;
    }

    /// Clears the stack, attempts to deserialize provided panel value into an actual panel and then pushes it into the stack
    pub async fn load_panels_from_value(&self, panels: Value) -> Result<(), JSONError> {
        self.required_feature("core_methods");
        match deserialize_panel(panels) {
            Ok(panel) => {
                let mut stack = self.current_stack().await;

                stack.clear();
                stack.push(panel.clone());
                drop(stack);

                self.send_core_event_to_modules(SDCoreEvent::StackReset {
                    new_panel: panel.clone()
                }, self.module_manager().get_module_list().await.into_iter()).await;

                self.core.mark_for_redraw().await;

                Ok(())
            }
            Err(err) => {
                Err(DeError::custom(format!("Failed to load panels: {}", err)))
            }
        }
    }

    /// Triggers button down event on all modules
    pub async fn button_down(&self, key: u8) {
        self.required_feature("core_methods");
        self.send_core_event_to_modules(SDCoreEvent::ButtonDown {
            key
        }, self.module_manager().get_module_list().await.into_iter()).await;
    }

    /// Triggers button up event on all modules
    pub async fn button_up(&self, key: u8) {
        self.required_feature("core_methods");
        self.send_core_event_to_modules(SDCoreEvent::ButtonUp {
            key
        }, self.module_manager().get_module_list().await.into_iter()).await;

        self.button_action(key).await;
    }

    /// Triggers button action event for modules that are related to components of the button
    pub async fn button_action(&self, key: u8) {
        self.required_feature("core_methods");
        if let Some(screen) = self.get_current_screen().await {
            let handle = screen.read().await;
            if let Some(button) = handle.buttons.get(&key).cloned() {
                drop(handle);

                let event = SDCoreEvent::ButtonAction {
                    key,
                    panel: screen.clone(),
                    pressed_button: button.clone()
                };

                self.send_core_event_to_modules(
                    event.clone(),
                    self.module_manager()
                        .get_modules_for_components(button.read().await.component_names().as_slice()).await
                        .into_iter()
                ).await;
                //send_event_to_socket(&self.core.socket_manager, core_event_to_global(event, &self.core.serial_number).await).await;

                self.core.mark_for_redraw().await;
            }
        }
    }

    /// Renders what current screen would look like into [DynamicImage] map
    pub async fn get_button_images(&self) -> Option<HashMap<u8, DynamicImage>> {
        let missing = draw_missing_texture(self.core.image_size);
        let custom = draw_custom_renderer_texture(self.core.image_size);
        let blank = image_from_solid(self.core.image_size, Rgba([0, 0, 0, 255]));

        let panel = self.get_current_screen().await?;
        let current_screen = panel.read().await;
        let buttons = current_screen.buttons.clone();

        let renderers = self.core.render_manager.read_renderers().await;

        let core_settings: CoreSettings = self.core.config.get_plugin_settings().await.unwrap_or_default();


        let mut images = HashMap::new();

        for (key, button) in buttons {
            if let Ok(component) = parse_unique_button_to_component::<RendererComponent>(&button).await {
                let modules = self.module_manager()
                    .get_modules_for_rendering(
                        &button.read().await.component_names()
                    ).await;
                let modules = modules.into_values()
                    .filter(|x| !component.plugin_blacklist.contains(&x.name()))
                    .filter(|x| !core_settings.renderer.plugin_blacklist.contains(&x.name()))
                    .collect::<Vec<UniqueSDModule>>();

                let image = if component.renderer.is_empty() {
                    draw_foreground(
                        &component,
                        &button,
                        &modules,
                        draw_background(
                            &component,
                            self,
                            &missing
                        ).await,
                        self
                    ).await
                } else {
                    if let Some(renderer) = renderers.get(&component.renderer) {
                        if let Some(image) = renderer.representation(key, &button, self).await {
                            image
                        } else {
                            custom.clone()
                        }
                    } else {
                        custom.clone()
                    }
                };

                images.insert(key, image);
            } else {
                images.insert(key, blank.clone());
            }
        }


        Some(images)
    }

    /// Renders what specified button would look like into [DynamicImage]
    pub async fn get_button_image(&self, key: u8) -> Option<DynamicImage> {
        let missing = draw_missing_texture(self.core.image_size);
        let custom = draw_custom_renderer_texture(self.core.image_size);
        let blank = image_from_solid(self.core.image_size, Rgba([0, 0, 0, 255]));

        let button = self.get_button(key).await?;
        let renderers = self.core.render_manager.read_renderers().await;

        let core_settings: CoreSettings = self.core.config.get_plugin_settings().await.unwrap_or_default();

        if let Ok(component) = parse_unique_button_to_component::<RendererComponent>(&button).await {
            let modules = self.module_manager().get_modules_for_rendering(&button.read().await.component_names()).await;
            let modules = modules.into_values()
                .filter(|x| !component.plugin_blacklist.contains(&x.name()))
                .filter(|x| !core_settings.renderer.plugin_blacklist.contains(&x.name()))
                .collect::<Vec<UniqueSDModule>>();

            let image = if component.renderer.is_empty() {
                draw_foreground(
                    &component,
                    &button,
                    &modules,
                    draw_background(
                        &component,
                        self,
                        &missing
                    ).await,
                    self
                ).await
            } else {
                if let Some(renderer) = renderers.get(&component.renderer) {
                    if let Some(image) = renderer.representation(key, &button, self).await {
                        image
                    } else {
                        custom.clone()
                    }
                } else {
                    custom.clone()
                }
            };

            Some(image)
        } else {
            Some(blank)
        }
    }

    /// Replaces current screen with specified one
    pub async fn replace_screen(&self, screen: ButtonPanel) {
        self.required_feature("core_methods");
        let mut stack = self.current_stack().await;

        let old_panel = stack.pop();
        stack.push(screen.clone());

        self.send_core_event_to_modules(SDCoreEvent::PanelReplaced {
            old_panel,
            new_panel: screen
        }, self.module_manager().get_module_list().await.into_iter()).await;

        self.core.mark_for_redraw().await;
    }

    /// Sets brightness of the streamdeck to specified (Range from 0 to 100)
    pub async fn set_brightness(&self, brightness: u8) {
        self.required_feature("core_methods");
        self.core.send_commands(vec![DeviceThreadCommunication::SetBrightness(brightness)]).await;

        let mut handle = self.core.device_config.write().await;
        handle.brightness = brightness;
    }

    /// Commits all changes to layout to device config so it can be later saved
    pub async fn commit_changes(&self) {
        self.required_feature("core_methods");
        let stack = self.get_root_screen().await;

        let mut handle = self.core.device_config.write().await;
        handle.layout = panel_to_raw(&stack).await;

        handle.dirty_state = true;
        handle.commit_time = Some(Instant::now());
        log::debug!("new commit to {}", handle.serial);
    }
}
