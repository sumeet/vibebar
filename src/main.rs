use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use iced::widget::{Space, container, image, mouse_area, row, svg};
use iced::{Background, Border, Color, Element, Length, Subscription, Theme};

use iced::window;
use iced_layershell::actions::{IcedNewPopupSettings, LayershellCustomAction, LayershellCustomActionWithId};
use iced_layershell::reexport::Anchor;
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::daemon;

use system_tray::client::{Client, Event, UpdateEvent};
use system_tray::item::IconPixmap;
use tokio::sync::mpsc;
use zbus::Connection;

// Channel for sending activation requests to the subscription (address, click_type, x, y)
static ACTIVATE_TX: OnceLock<mpsc::UnboundedSender<(String, ClickType, i32, i32)>> = OnceLock::new();

// Design constants
const BAR_BG: Color = Color::from_rgb(9.0 / 255.0, 9.0 / 255.0, 11.0 / 255.0);
const ICON_SIZE: f32 = 22.0;
const CONTAINER_SIZE: f32 = 26.0;

#[derive(Debug, Clone)]
struct IconData {
    pixmap: Option<Vec<IconPixmap>>,
    icon_name: Option<String>,
    icon_theme_path: Option<String>,
}

#[derive(Debug, Clone)]
enum TrayEvent {
    Add { address: String, icon: IconData },
    Update { address: String, icon: IconData },
    Remove { address: String },
    Tick, // Used for internal state machine transitions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickType {
    Left,
    Right,
    Middle,
}

// Manual Message enum - NOT using to_layer_message macro so we can control popup parenting
#[derive(Debug, Clone)]
enum Message {
    Tray(TrayEvent),
    TrayIconClicked(String, ClickType), // address, click type
    TrayIconHover(String, bool),         // address, is_hovered
    MouseMoved(iced::Point),
    ClosePopup,
    WindowResized(window::Id, iced::Size),
    // Layershell actions with explicit parent control
    OpenPopup { parent: window::Id, popup: window::Id, settings: IcedNewPopupSettings },
    CloseWindow(window::Id),
}

// Manual TryInto impl to specify parent ID for popups
impl TryInto<LayershellCustomActionWithId> for Message {
    type Error = Self;

    fn try_into(self) -> Result<LayershellCustomActionWithId, Self::Error> {
        match self {
            Message::OpenPopup { parent, popup, settings } => Ok(
                LayershellCustomActionWithId::new(
                    Some(parent),
                    LayershellCustomAction::NewPopUp { settings, id: popup },
                )
            ),
            Message::CloseWindow(id) => Ok(
                LayershellCustomActionWithId::new(
                    Some(id),
                    LayershellCustomAction::RemoveWindow,
                )
            ),
            other => Err(other),
        }
    }
}

enum IconHandle {
    Raster(image::Handle),
    Svg(svg::Handle),
}

struct TrayItem {
    icon: Option<IconHandle>,
    hovered: bool,
}

struct State {
    tray_items: HashMap<String, TrayItem>,
    mouse_position: (f32, f32),
    main_bar_id: Option<window::Id>,   // The main bar window ID (for parenting popups)
    bar_width: u32,                    // Actual bar width from Resized events
    active_popup: Option<window::Id>,  // Current popup window (only one at a time)
    popup_for_address: Option<String>, // Which tray item's popup is open
}

fn init() -> (State, iced::Task<Message>) {
    (
        State {
            tray_items: HashMap::new(),
            mouse_position: (0.0, 0.0),
            main_bar_id: None, // Will be set on first Resized event
            bar_width: 1920,   // Default, will be updated on first Resized event
            active_popup: None,
            popup_for_address: None,
        },
        iced::Task::none(),
    )
}

fn namespace() -> String {
    "vibebar".to_string()
}

fn update(state: &mut State, msg: Message) -> iced::Task<Message> {
    match msg {
        Message::Tray(event) => match event {
            TrayEvent::Add { address, icon } | TrayEvent::Update { address, icon } => {
                let icon_handle = resolve_icon(&icon);
                let hovered = state.tray_items.get(&address).map(|i| i.hovered).unwrap_or(false);
                state
                    .tray_items
                    .insert(address, TrayItem { icon: icon_handle, hovered });
            }
            TrayEvent::Remove { address } => {
                state.tray_items.remove(&address);
            }
            TrayEvent::Tick => {}
        },
        Message::TrayIconClicked(address, click_type) => {
            match click_type {
                ClickType::Right => {
                    // Need main bar ID to parent the popup
                    let Some(parent) = state.main_bar_id else {
                        eprintln!("No main bar ID yet, can't open popup");
                        return iced::Task::none();
                    };

                    // Close any existing popup first
                    let close_task = if let Some(existing_id) = state.active_popup.take() {
                        state.popup_for_address = None;
                        iced::Task::done(Message::CloseWindow(existing_id))
                    } else {
                        iced::Task::none()
                    };

                    // Open a popup menu below the icon
                    let popup = window::Id::unique();
                    state.active_popup = Some(popup);
                    state.popup_for_address = Some(address);

                    // Position: center below the clicked icon, clamped to bar width
                    let menu_width = 200i32;
                    let menu_height = 80i32;
                    let bar_w = state.bar_width as i32;
                    let (mouse_x, _mouse_y) = state.mouse_position;
                    let margin = 4i32;

                    // Prefer centered under click, clamp to bar edges
                    let prefer_center = (mouse_x as i32) - (menu_width / 2);
                    let min_x = margin;
                    let max_x = bar_w - menu_width - margin;
                    let x = prefer_center.clamp(min_x, max_x.max(min_x));

                    let y = 30 + 6; // bar height + gap

                    let open_task = iced::Task::done(Message::OpenPopup {
                        parent,
                        popup,
                        settings: IcedNewPopupSettings {
                            size: (menu_width as u32, menu_height as u32),
                            position: (x, y),
                        },
                    });

                    return iced::Task::batch([close_task, open_task]);
                }
                _ => {
                    // Left and middle click - send to DBus
                    if let Some(tx) = ACTIVATE_TX.get() {
                        let (x, y) = state.mouse_position;
                        let _ = tx.send((address, click_type, x as i32, y as i32));
                    }
                }
            }
        }
        Message::ClosePopup => {
            if let Some(id) = state.active_popup.take() {
                state.popup_for_address = None;
                return iced::Task::done(Message::CloseWindow(id));
            }
        }
        Message::WindowResized(id, size) => {
            // Capture the main bar ID from the first window event (bar is first window)
            if state.main_bar_id.is_none() && size.width > 100.0 {
                state.main_bar_id = Some(id);
                eprintln!("Captured main bar ID: {:?}, width: {}", id, size.width);
            }
            // Only track bar width from the main window, not popups
            if state.main_bar_id == Some(id) {
                state.bar_width = size.width as u32;
            }
        }
        Message::TrayIconHover(address, is_hovered) => {
            if let Some(item) = state.tray_items.get_mut(&address) {
                item.hovered = is_hovered;
            }
        }
        Message::MouseMoved(point) => {
            state.mouse_position = (point.x, point.y);
        }
        // OpenPopup and CloseWindow are handled by TryInto -> layershell, not here
        Message::OpenPopup { .. } | Message::CloseWindow(_) => {}
    }
    iced::Task::none()
}

fn resolve_icon(icon: &IconData) -> Option<IconHandle> {
    // Prefer pixmap if available (pick largest for quality)
    if let Some(ref pixmaps) = icon.pixmap {
        if !pixmaps.is_empty() {
            return pixmap_to_handle(pixmaps).map(IconHandle::Raster);
        }
    }

    // Fall back to icon_name lookup
    if let Some(ref name) = icon.icon_name {
        if !name.is_empty() {
            return lookup_icon(name, icon.icon_theme_path.as_deref());
        }
    }

    None
}

fn pixmap_to_handle(pixmaps: &[IconPixmap]) -> Option<image::Handle> {
    // Pick the LARGEST pixmap for best quality (iced will downscale)
    let pixmap = pixmaps.iter().max_by_key(|p| p.width * p.height)?;

    // Convert ARGB to RGBA
    let mut rgba = Vec::with_capacity(pixmap.pixels.len());
    for chunk in pixmap.pixels.chunks(4) {
        if chunk.len() == 4 {
            let [a, r, g, b] = [chunk[0], chunk[1], chunk[2], chunk[3]];
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    Some(image::Handle::from_rgba(
        pixmap.width as u32,
        pixmap.height as u32,
        rgba,
    ))
}

fn lookup_icon(name: &str, theme_path: Option<&str>) -> Option<IconHandle> {
    // Try freedesktop icon lookup - request large size for quality
    let path = freedesktop_icons::lookup(name)
        .with_size(128) // Request large, iced will scale down
        .with_cache()
        .find();

    let path = path.or_else(|| {
        if let Some(tp) = theme_path {
            let candidates = [
                format!("{}/{}.svg", tp, name),
                format!("{}/{}.png", tp, name),
                format!("{}/hicolor/scalable/apps/{}.svg", tp, name),
                format!("{}/hicolor/256x256/apps/{}.png", tp, name),
                format!("{}/hicolor/128x128/apps/{}.png", tp, name),
                format!("{}/hicolor/64x64/apps/{}.png", tp, name),
            ];
            for c in candidates {
                let p = PathBuf::from(&c);
                if p.exists() {
                    return Some(p);
                }
            }
        }
        None
    });

    let path = path?;
    load_icon_file(&path)
}

fn load_icon_file(path: &PathBuf) -> Option<IconHandle> {
    let ext = path.extension()?.to_str()?;

    match ext.to_lowercase().as_str() {
        "svg" => Some(IconHandle::Svg(svg::Handle::from_path(path))),
        "png" => load_png(path).map(IconHandle::Raster),
        _ => None,
    }
}

fn load_png(path: &PathBuf) -> Option<image::Handle> {
    let data = std::fs::read(path).ok()?;
    let img = image_crate::load_from_memory(&data).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();

    Some(image::Handle::from_rgba(w, h, rgba.into_raw()))
}

fn tray_icon_container_style(hovered: bool) -> container::Style {
    if hovered {
        container::Style {
            background: Some(Background::Color(Color::from_rgba(1.0, 1.0, 1.0, 0.20))),
            border: Border {
                radius: 8.0.into(),
                width: 1.0,
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.50),
            },
            ..Default::default()
        }
    } else {
        container::Style::default()
    }
}

fn view(state: &State, window_id: window::Id) -> Element<'_, Message> {
    // Only render bar for the main bar window - anything else gets popup view
    // This prevents flickering where unknown windows briefly show bar content
    if state.main_bar_id != Some(window_id) {
        return view_popup(state);
    }

    // Main bar view
    let tray_icons: Vec<Element<'_, Message>> = state
        .tray_items
        .iter()
        .filter_map(|(address, item)| {
            item.icon.as_ref().map(|handle| {
                let icon_widget: Element<'_, Message> = match handle {
                    IconHandle::Raster(h) => image(h.clone())
                        .width(Length::Fixed(ICON_SIZE))
                        .height(Length::Fixed(ICON_SIZE))
                        .into(),
                    IconHandle::Svg(h) => svg(h.clone())
                        .width(Length::Fixed(ICON_SIZE))
                        .height(Length::Fixed(ICON_SIZE))
                        .into(),
                };

                let hovered = item.hovered;
                let addr = address.clone();
                let addr2 = address.clone();
                let addr3 = address.clone();
                let addr4 = address.clone();
                let addr5 = address.clone();

                mouse_area(
                    container(icon_widget)
                        .width(Length::Fixed(CONTAINER_SIZE))
                        .height(Length::Fixed(CONTAINER_SIZE))
                        .center_x(Length::Fixed(CONTAINER_SIZE))
                        .center_y(Length::Fixed(CONTAINER_SIZE))
                        .style(move |_| tray_icon_container_style(hovered)),
                )
                .on_press(Message::TrayIconClicked(addr, ClickType::Left))
                .on_right_press(Message::TrayIconClicked(addr2, ClickType::Right))
                .on_middle_press(Message::TrayIconClicked(addr3, ClickType::Middle))
                .on_enter(Message::TrayIconHover(addr4, true))
                .on_exit(Message::TrayIconHover(addr5, false))
                .into()
            })
        })
        .collect();

    let tray_row = row(tray_icons).spacing(4);

    container(
        row![
            Space::new().width(Length::Fill),
            Space::new().width(Length::Fixed(24.0)),
            tray_row,
            Space::new().width(Length::Fixed(10.0)),
        ]
        .align_y(iced::Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(BAR_BG.into()),
        ..Default::default()
    })
    .into()
}

// Dark Prism menu colors
const MENU_BG: Color = Color::from_rgb(24.0 / 255.0, 24.0 / 255.0, 27.0 / 255.0);
const MENU_TEXT: Color = Color::from_rgb(244.0 / 255.0, 244.0 / 255.0, 245.0 / 255.0);
const MENU_BORDER: Color = Color::from_rgba(255.0 / 255.0, 255.0 / 255.0, 255.0 / 255.0, 0.1);

fn view_popup(state: &State) -> Element<'_, Message> {
    use iced::widget::{button, column, text};

    let label = state.popup_for_address
        .as_ref()
        .map(|a| format!("Menu for {}", a))
        .unwrap_or_else(|| "Menu".to_string());

    // Single container fills the window with rounded corners
    // The transparent app background allows corners to show through
    container(
        column![
            text(label).size(12).color(MENU_TEXT),
            button(text("Close").size(12).color(MENU_TEXT))
                .on_press(Message::ClosePopup)
                .padding(4),
        ]
        .spacing(6)
        .padding(8),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(MENU_BG)),
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: MENU_BORDER,
        },
        ..Default::default()
    })
    .into()
}

fn theme(_state: &State, _window_id: window::Id) -> Theme {
    Theme::Dark
}

// App style with transparent background (allows rounded corners on popups)
fn style(_state: &State, theme: &Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    }
}


async fn lookup_full_sni_address(bus_name: &str) -> zbus::Result<String> {
    let conn = Connection::session().await?;
    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&conn)
        .destination("org.kde.StatusNotifierWatcher")?
        .path("/StatusNotifierWatcher")?
        .interface("org.kde.StatusNotifierWatcher")?
        .build()
        .await?;

    let items: Vec<String> = proxy.get_property("RegisteredStatusNotifierItems").await?;

    // Find the item that starts with our bus name
    for item in items {
        if item.starts_with(bus_name) {
            return Ok(item);
        }
    }

    // Fallback to default path
    Ok(format!("{}/StatusNotifierItem", bus_name))
}

fn parse_sni_address(address: &str) -> (&str, String) {
    // Address can be ":1.58" or ":1.58/org/blueman/sni"
    address
        .split_once('/')
        .map_or((address, String::from("/StatusNotifierItem")), |(d, p)| {
            (d, format!("/{p}"))
        })
}

async fn sni_activate(bus_name: &str, x: i32, y: i32) -> zbus::Result<()> {
    let full_address = lookup_full_sni_address(bus_name).await?;
    let (dest, path) = parse_sni_address(&full_address);

    let conn = Connection::session().await?;
    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&conn)
        .destination(dest)?
        .path(path.as_str())?
        .interface("org.kde.StatusNotifierItem")?
        .build()
        .await?;

    proxy.call::<_, (i32, i32), ()>("Activate", &(x, y)).await?;
    Ok(())
}

async fn sni_context_menu(bus_name: &str, x: i32, y: i32) -> zbus::Result<()> {
    let full_address = lookup_full_sni_address(bus_name).await?;
    let (dest, path) = parse_sni_address(&full_address);

    let conn = Connection::session().await?;
    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&conn)
        .destination(dest)?
        .path(path.as_str())?
        .interface("org.kde.StatusNotifierItem")?
        .build()
        .await?;

    proxy.call::<_, (i32, i32), ()>("ContextMenu", &(x, y)).await?;
    Ok(())
}

async fn sni_secondary_activate(bus_name: &str, x: i32, y: i32) -> zbus::Result<()> {
    let full_address = lookup_full_sni_address(bus_name).await?;
    let (dest, path) = parse_sni_address(&full_address);

    let conn = Connection::session().await?;
    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&conn)
        .destination(dest)?
        .path(path.as_str())?
        .interface("org.kde.StatusNotifierItem")?
        .build()
        .await?;

    proxy.call::<_, (i32, i32), ()>("SecondaryActivate", &(x, y)).await?;
    Ok(())
}

fn subscription(_state: &State) -> Subscription<Message> {
    Subscription::batch([
        Subscription::run(tray_subscription),
        iced::event::listen_with(|event, _status, id| {
            match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::MouseMoved(position))
                }
                iced::Event::Window(iced::window::Event::Resized(size)) => {
                    Some(Message::WindowResized(id, size))
                }
                iced::Event::Window(iced::window::Event::Opened { size, .. }) => {
                    Some(Message::WindowResized(id, size))
                }
                _ => None
            }
        }),
    ])
}

fn tray_subscription() -> impl iced::futures::Stream<Item = Message> {
    iced::futures::stream::unfold(TrayState::Disconnected, |state| async move {
        match state {
            TrayState::Disconnected => match Client::new().await {
                Ok(client) => {
                    let rx = client.subscribe();

                    // Create activation channel and register sender
                    let (activate_tx, activate_rx) = mpsc::unbounded_channel();
                    let _ = ACTIVATE_TX.set(activate_tx);

                    let initial: Vec<_> = {
                        let items = client.items();
                        let guard = items.lock().unwrap();
                        guard
                            .iter()
                            .map(|(address, (item, _menu))| {
                                let icon = IconData {
                                    pixmap: item.icon_pixmap.clone(),
                                    icon_name: item.icon_name.clone(),
                                    icon_theme_path: item.icon_theme_path.clone(),
                                };
                                (address.clone(), icon)
                            })
                            .collect()
                    };

                    Some((
                        Message::Tray(TrayEvent::Tick),
                        TrayState::SendingInitial {
                            client,
                            rx,
                            activate_rx,
                            initial,
                            index: 0,
                        },
                    ))
                }
                Err(e) => {
                    eprintln!("Failed to connect to system tray: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    Some((Message::Tray(TrayEvent::Tick), TrayState::Disconnected))
                }
            },
            TrayState::SendingInitial {
                client,
                rx,
                activate_rx,
                initial,
                index,
            } => {
                if index < initial.len() {
                    let (address, icon) = initial[index].clone();
                    Some((
                        Message::Tray(TrayEvent::Add { address, icon }),
                        TrayState::SendingInitial {
                            client,
                            rx,
                            activate_rx,
                            initial,
                            index: index + 1,
                        },
                    ))
                } else {
                    Some((
                        Message::Tray(TrayEvent::Tick),
                        TrayState::Connected { client, rx, activate_rx },
                    ))
                }
            }
            TrayState::Connected { client, mut rx, mut activate_rx } => {
                tokio::select! {
                    // Handle tray events
                    event_result = rx.recv() => {
                        match event_result {
                            Ok(event) => {
                                let tray_event = match event {
                                    Event::Add(address, item) => {
                                        let icon = IconData {
                                            pixmap: item.icon_pixmap.clone(),
                                            icon_name: item.icon_name.clone(),
                                            icon_theme_path: item.icon_theme_path.clone(),
                                        };
                                        TrayEvent::Add { address, icon }
                                    }
                                    Event::Update(address, update) => match update {
                                        UpdateEvent::Icon {
                                            icon_name,
                                            icon_pixmap,
                                        } => {
                                            let icon = IconData {
                                                pixmap: icon_pixmap,
                                                icon_name,
                                                icon_theme_path: None,
                                            };
                                            TrayEvent::Update { address, icon }
                                        }
                                        _ => {
                                            return Some((
                                                Message::Tray(TrayEvent::Tick),
                                                TrayState::Connected { client, rx, activate_rx },
                                            ));
                                        }
                                    },
                                    Event::Remove(address) => TrayEvent::Remove { address },
                                };
                                Some((
                                    Message::Tray(tray_event),
                                    TrayState::Connected { client, rx, activate_rx },
                                ))
                            }
                            Err(e) => {
                                eprintln!("Tray subscription error: {e}");
                                Some((Message::Tray(TrayEvent::Tick), TrayState::Disconnected))
                            }
                        }
                    }
                    // Handle activation requests from UI
                    Some((address, click_type, x, y)) = activate_rx.recv() => {
                        match click_type {
                            ClickType::Left => {
                                // Check item_is_menu flag
                                let item_is_menu = {
                                    let items = client.items();
                                    let guard = items.lock().unwrap();
                                    guard.get(&address)
                                        .map(|(item, _)| item.item_is_menu)
                                        .unwrap_or(false)
                                };
                                if item_is_menu {
                                    let _ = sni_context_menu(&address, x, y).await;
                                } else {
                                    if sni_activate(&address, x, y).await.is_err() {
                                        let _ = sni_context_menu(&address, x, y).await;
                                    }
                                }
                            }
                            ClickType::Right => {
                                let _ = sni_context_menu(&address, x, y).await;
                            }
                            ClickType::Middle => {
                                let _ = sni_secondary_activate(&address, x, y).await;
                            }
                        }
                        Some((
                            Message::Tray(TrayEvent::Tick),
                            TrayState::Connected { client, rx, activate_rx },
                        ))
                    }
                }
            }
        }
    })
}

enum TrayState {
    Disconnected,
    SendingInitial {
        client: Client,
        rx: tokio::sync::broadcast::Receiver<Event>,
        activate_rx: mpsc::UnboundedReceiver<(String, ClickType, i32, i32)>,
        initial: Vec<(String, IconData)>,
        index: usize,
    },
    Connected {
        client: Client,
        rx: tokio::sync::broadcast::Receiver<Event>,
        activate_rx: mpsc::UnboundedReceiver<(String, ClickType, i32, i32)>,
    },
}

pub fn main() -> Result<(), iced_layershell::Error> {
    daemon(init, namespace, update, view)
        .style(style)
        .theme(theme)
        .subscription(subscription)
        .settings(Settings {
            layer_settings: LayerShellSettings {
                size: Some((0, 30)),
                exclusive_zone: 30,
                anchor: Anchor::Top | Anchor::Left | Anchor::Right,
                start_mode: StartMode::Active,
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}
