//! Top-level window enumeration (ICCCM/EWMH-lite).

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, MapState, Window,
};

#[derive(Debug, Clone)]
pub struct TopLevel {
    pub window: Window,
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
    pub mapped: bool,
    pub name: String,
}

/// Window classification for the mini-WM: what kind of top-level this is,
/// which window it belongs to, and whether the app manages it itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowInfo {
    pub kind: u8,
    pub parent: Option<Window>,
    pub override_redirect: bool,
}

fn atom<C: Connection>(conn: &C, name: &[u8]) -> Option<u32> {
    conn.intern_atom(false, name).ok()?.reply().ok().map(|r| r.atom)
}

/// Classify a window from `override_redirect`, `_NET_WM_WINDOW_TYPE` and
/// `WM_TRANSIENT_FOR`. Unknown/absent types fall back to normal; bare
/// override-redirect windows are treated as popups (menus/tooltips).
pub fn window_info<C: Connection>(conn: &C, win: Window) -> WindowInfo {
    let override_redirect = conn
        .get_window_attributes(win)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|a| a.override_redirect)
        .unwrap_or(false);

    let parent = atom(conn, b"WM_TRANSIENT_FOR").and_then(|a| {
        let reply = conn
            .get_property(false, win, a, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply
            .value32()
            .and_then(|mut it| it.next())
            .filter(|w| *w != 0)
    });

    let mut kind = None;
    if let Some(type_atom) = atom(conn, b"_NET_WM_WINDOW_TYPE") {
        let types = conn
            .get_property(false, win, type_atom, AtomEnum::ATOM, 0, 16)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|it| it.collect::<Vec<u32>>()));
        if let Some(types) = types {
            {
                let dialog = atom(conn, b"_NET_WM_WINDOW_TYPE_DIALOG");
                let menu = atom(conn, b"_NET_WM_WINDOW_TYPE_MENU");
                let dropdown = atom(conn, b"_NET_WM_WINDOW_TYPE_DROPDOWN_MENU");
                let popup = atom(conn, b"_NET_WM_WINDOW_TYPE_POPUP_MENU");
                let combo = atom(conn, b"_NET_WM_WINDOW_TYPE_COMBO");
                let tooltip = atom(conn, b"_NET_WM_WINDOW_TYPE_TOOLTIP");
                let utility = atom(conn, b"_NET_WM_WINDOW_TYPE_UTILITY");
                let toolbar = atom(conn, b"_NET_WM_WINDOW_TYPE_TOOLBAR");
                let splash = atom(conn, b"_NET_WM_WINDOW_TYPE_SPLASH");
                let normal = atom(conn, b"_NET_WM_WINDOW_TYPE_NORMAL");
                for t in types {
                    let t = Some(t);
                    if t == dialog {
                        kind = Some(xcom_proto::KIND_DIALOG);
                    } else if t == menu || t == dropdown || t == popup || t == combo {
                        kind = Some(xcom_proto::KIND_MENU);
                    } else if t == tooltip {
                        kind = Some(xcom_proto::KIND_TOOLTIP);
                    } else if t == utility || t == toolbar {
                        kind = Some(xcom_proto::KIND_UTILITY);
                    } else if t == splash {
                        kind = Some(xcom_proto::KIND_SPLASH);
                    } else if t == normal {
                        kind = Some(xcom_proto::KIND_NORMAL);
                    }
                    if kind.is_some() {
                        break;
                    }
                }
            }
        }
    }

    let kind = kind.unwrap_or(if override_redirect {
        xcom_proto::KIND_MENU
    } else {
        xcom_proto::KIND_NORMAL
    });

    WindowInfo {
        kind,
        parent,
        override_redirect,
    }
}

/// Children of `root` that look like application top-levels.
///
/// Override-redirect windows (menus, tooltips, drags) are excluded unless
/// `include_override_redirect` is set: the capture session wants everything,
/// while window selection UIs usually don't.
pub fn list_top_levels<C: Connection>(
    conn: &C,
    root: Window,
    include_override_redirect: bool,
) -> Result<Vec<TopLevel>> {
    let tree = conn.query_tree(root)?.reply()?;
    let mut out = Vec::new();
    for win in tree.children {
        let attrs = match conn.get_window_attributes(win)?.reply() {
            Ok(a) => a,
            Err(_) => continue,
        };
        if attrs.override_redirect && !include_override_redirect {
            continue;
        }
        let geom = match conn.get_geometry(win)?.reply() {
            Ok(g) => g,
            Err(_) => continue,
        };
        out.push(TopLevel {
            window: win,
            x: geom.x,
            y: geom.y,
            w: geom.width,
            h: geom.height,
            mapped: attrs.map_state == MapState::VIEWABLE,
            name: window_name(conn, win).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// `_NET_WM_NAME` (UTF-8) with `WM_NAME` fallback.
pub fn window_name<C: Connection>(conn: &C, win: Window) -> Result<String> {
    let net_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
    if let Some(s) = text_prop(conn, win, net_name) {
        if !s.is_empty() {
            return Ok(s);
        }
    }
    if let Some(s) = text_prop(conn, win, AtomEnum::WM_NAME.into()) {
        if !s.is_empty() {
            return Ok(s);
        }
    }
    Ok(String::new())
}

fn text_prop<C: Connection>(conn: &C, win: Window, prop: Atom) -> Option<String> {
    let reply = conn
        .get_property(false, win, prop, AtomEnum::ANY, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&reply.value).into_owned())
}

/// Ask a window to close: ICCCM `WM_DELETE_WINDOW` when the client supports it,
/// otherwise destroy the window (there is no window manager to mediate).
///
/// Returns `true` when the polite path was used.
pub fn request_close<C: Connection>(conn: &C, win: Window) -> Result<bool> {
    let protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
    let delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
    let supported = conn
        .get_property(false, win, protocols, AtomEnum::ATOM, 0, 64)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|mut it| it.any(|a| a == delete)))
        .unwrap_or(false);
    if supported {
        let ev = ClientMessageEvent::new(32, win, protocols, [delete, 0, 0, 0, 0]);
        conn.send_event(false, win, EventMask::NO_EVENT, ev)?;
    } else {
        conn.destroy_window(win)?;
    }
    conn.flush()?;
    Ok(supported)
}

/// Find a mapped top-level whose name contains `needle` (case-insensitive).
pub fn find_by_name<C: Connection>(
    conn: &C,
    root: Window,
    needle: &str,
) -> Result<Option<TopLevel>> {
    let needle = needle.to_lowercase();
    let tops = list_top_levels(conn, root, false)?;
    Ok(tops
        .into_iter()
        .find(|t| t.mapped && t.name.to_lowercase().contains(&needle)))
}
