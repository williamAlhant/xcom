//! Top-level window enumeration (ICCCM/EWMH-lite).

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, MapState, Window};

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
