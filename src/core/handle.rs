//! XEvent handlers for use in the main event loop;
use crate::{
    core::{
        bindings::{KeyBindings, KeyCode, MouseBindings, MouseEvent},
        State, Xid,
    },
    pure::{geometry::Point, Workspace},
    x::{
        atom::Atom,
        event::{ClientMessage, ClientMessageKind, PointerChange},
        property::{Prop, WmHints},
        XConn, XConnExt,
    },
    Result, StackSet,
};
use std::mem::take;
use tracing::{error, info, trace};

// fn is_fullscreen<X>(data: &[u32], x: &X) -> bool
// where
//     X: XConnExt,
// {
//     data.iter()
//         .map(|&a| x.atom_name(Xid(a)))
//         .flatten()
//         .any(|s| s == Atom::NetWmStateFullscreen.as_ref())
// }

// Currently no client messages are handled by default (see the ewmh extension for some examples of messages
// that are handled when that is enabled)
pub(crate) fn client_message<X: XConn>(msg: ClientMessage, _: &mut State<X>, _: &X) -> Result<()> {
    let data = &msg.data;
    trace!(id = msg.id.0, dtype = ?msg.dtype, ?data, "got client message");

    Ok(())
}

pub(crate) fn keypress<X: XConn>(
    key: KeyCode,
    bindings: &mut KeyBindings<X>,
    state: &mut State<X>,
    x: &X,
) -> Result<()> {
    if let Some(action) = bindings.get_mut(&key) {
        trace!(?key, "running user keybinding");
        if let Err(error) = action.call(state, x) {
            error!(%error, ?key, "error running user keybinding");
            return Err(error);
        }
    }

    Ok(())
}

pub(crate) fn mouse_event<X: XConn>(
    e: MouseEvent,
    bindings: &mut MouseBindings<X>,
    state: &mut State<X>,
    x: &X,
) -> Result<()> {
    if let Some(action) = bindings.get_mut(&(e.kind, e.state.clone())) {
        if let Err(error) = action.call(&e, state, x) {
            error!(%error, ?e, "error running user mouse binding");
            return Err(error);
        }
    }

    Ok(())
}

pub(crate) fn map_request<X: XConn>(client: Xid, state: &mut State<X>, x: &X) -> Result<()> {
    trace!(?client, "handling new map request");
    let attrs = x.get_window_attributes(client)?;

    if !state.client_set.contains(&client) && !attrs.override_redirect {
        trace!(?client, "managing client");
        x.manage(client, state)?;
    }

    Ok(())
}

pub(crate) fn destroy<X: XConn>(client: Xid, state: &mut State<X>, x: &X) -> Result<()> {
    if state.client_set.contains(&client) {
        trace!(?client, "destroying client");
        x.unmanage(client, state)?;
        state.mapped.remove(&client);
        state.pending_unmap.remove(&client);
    }

    Ok(())
}

// Expected unmap events are tracked in pending_unmap. We ignore expected unmaps.
pub(crate) fn unmap_notify<X: XConn>(client: Xid, state: &mut State<X>, x: &X) -> Result<()> {
    let expected = *state.pending_unmap.get(&client).unwrap_or(&0);

    if expected == 0 {
        x.unmanage(client, state)?;
    } else if expected == 1 {
        state.pending_unmap.remove(&client);
    } else {
        state
            .pending_unmap
            .entry(client)
            .and_modify(|count| *count -= 1);
    }

    Ok(())
}

pub(crate) fn focus_in<X: XConn>(client: Xid, state: &mut State<X>, x: &X) -> Result<()> {
    let accepts_focus = match x.get_prop(client, Atom::WmHints.as_ref()) {
        Ok(Some(Prop::WmHints(WmHints { accepts_input, .. }))) => accepts_input,
        _ => true,
    };

    if accepts_focus {
        x.focus(client)?;
        x.set_prop(
            x.root(),
            Atom::NetActiveWindow.as_ref(),
            Prop::Window(vec![client]),
        )?;
        x.set_active_client(client, state)?;
    } else {
        let msg = ClientMessageKind::TakeFocus(client).as_message(x)?;
        x.send_client_message(msg)?;
    }

    Ok(())
}

pub(crate) fn enter<X: XConn>(p: PointerChange, state: &mut State<X>, x: &X) -> Result<()> {
    if state.config.focus_follow_mouse {
        x.modify_and_refresh(state, |cs| {
            cs.focus_client(&p.id);
        })
    } else {
        Ok(())
    }
}

pub(crate) fn leave<X: XConn>(p: PointerChange, state: &mut State<X>, x: &X) -> Result<()> {
    if p.id == state.root() && !p.same_screen {
        x.focus(p.id)?;
        set_screen_from_point(p.abs, state, x)?;
    }

    Ok(())
}

pub(crate) fn detect_screens<X: XConn>(state: &mut State<X>, x: &X) -> Result<()> {
    info!("re-detecting screens");
    let rects = x.screen_details()?;
    info!(?rects, "found screens");

    let StackSet {
        screens,
        hidden,
        floating,
        previous_tag,
        invisible_tags,
    } = take(&mut state.client_set);

    let mut workspaces: Vec<_> = screens.into_iter().map(|s| s.workspace).collect();
    workspaces.extend(hidden);

    // Pad out the workspace list with default workspaces if there aren't enough available
    // to cover the attached screens.
    // NOTE: This can still error if we end up with a tag collision because the user has
    //       named one of there tags with the one we generate based on ID.
    if workspaces.len() < rects.len() {
        let n_short = rects.len() - workspaces.len();
        let next_id = workspaces.iter().map(|w| w.id).max().unwrap_or(0) + 1;
        workspaces.extend((0..n_short).map(|n| Workspace::new_default(n + next_id)))
    }

    state.client_set = StackSet {
        previous_tag,
        invisible_tags,
        ..StackSet::try_new_concrete(workspaces, rects, floating)?
    };

    Ok(())
}

pub(crate) fn screen_change<X: XConn>(state: &mut State<X>, x: &X) -> Result<()> {
    trace!("screen changed");
    set_screen_from_point(x.cursor_position()?, state, x)
}

fn set_screen_from_point<X: XConn>(p: Point, state: &mut State<X>, x: &X) -> Result<()> {
    x.modify_and_refresh(state, |cs| {
        let index = cs
            .screens()
            .find(|s| s.r.contains_point(p))
            .map(|s| s.index());

        if let Some(index) = index {
            cs.focus_screen(index);
        }
    })
}
