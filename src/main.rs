use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use iced::widget::{Space, container, image, mouse_area, row, svg};
use iced::{Background, Border, Color, Element, Length, Subscription, Theme};

use iced_layershell::application;
use iced_layershell::reexport::Anchor;
use iced_layershell::settings::{LayerShellSettings, Settings, StartMode};
use iced_layershell::to_layer_message;

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

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Tray(TrayEvent),
    TrayIconClicked(String, ClickType), // address, click type
    TrayIconHover(String, bool),         // address, is_hovered
    MouseMoved(iced::Point),
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
}

fn init() -> (State, iced::Task<Message>) {
    (
        State {
            tray_items: HashMap::new(),
            mouse_position: (0.0, 0.0),
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
            if let Some(tx) = ACTIVATE_TX.get() {
                let (x, y) = state.mouse_position;
                let _ = tx.send((address, click_type, x as i32, y as i32));
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
        _ => {}
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

fn view(state: &State) -> Element<'_, Message> {
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

fn theme(_state: &State) -> Theme {
    Theme::Dark
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
        iced::event::listen_with(|event, _status, _id| {
            if let iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) = event {
                Some(Message::MouseMoved(position))
            } else {
                None
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
    application(init, namespace, update, view)
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
