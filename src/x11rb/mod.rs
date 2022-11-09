//! Helpers and utilities for using x11rb as a back end for penrose
//!
//! Docs for the `X11` core protocol can be found [here][1]. x11rb is a thin facade over this. For
//! X11 extensions, there are usually separate documentations. For example, the RandR extension is
//! documented in [2].
//!
//! This module contains the code for talking to the X11 server using the x11rb crate, which
//! offers an implementation of the X11 protocol in safe Rust. The actual protocol bindings are
//! autogenerated from an XML spec. The XML files can be found [in its xcb-proto-{something}
//! subfolder](https://github.com/psychon/x11rb) and are useful as a reference for how the API
//! works. x11rb also [offers](https://github.com/psychon/x11rb/blob/master/doc/generated_code.md)
//! some explanation on how the XML is turned into Rust code.
//!
//! The original implementation of this was by @psychon (Uli Schlachter).
//! Re-write for the new 0.3.0 API by @sminez (Innes Anderson-Morrison).
//!
//! [1]: https://www.x.org/releases/X11R7.6/doc/xproto/x11protocol.html
//! [2]: https://gitlab.freedesktop.org/xorg/proto/randrproto/-/blob/master/randrproto.txt
use crate::{
    core::bindings::{KeyCode, MouseState},
    pure::geometry::{Point, Rect},
    x::{
        self,
        atom::Atom,
        event::{ClientEventMask, ClientMessage},
        property::{Prop, WindowAttributes, WmHints, WmNormalHints, WmState},
        ClientAttr, ClientConfig, WinType, XConn, XEvent,
    },
    Error, Result, Xid,
};
use std::{collections::HashMap, convert::TryFrom, str::FromStr};
use strum::IntoEnumIterator;
use tracing::error;
use x11rb::{
    connection::Connection,
    protocol::{
        randr::{self, ConnectionExt as _, NotifyMask},
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent,
            ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask,
            GrabMode, InputFocus, MapState, ModMask, PropMode, StackMode, WindowClass,
            CLIENT_MESSAGE_EVENT,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
    CURRENT_TIME,
};

pub mod conversions;

use conversions::convert_event;

const RANDR_VER: (u32, u32) = (1, 2);

#[derive(Debug)]
pub(crate) struct Atoms {
    atoms: HashMap<Atom, u32>,
}

impl Atoms {
    pub(crate) fn new(conn: &impl Connection) -> Result<Self> {
        // First send all requests...
        let atom_requests = Atom::iter()
            .map(|atom| Ok((atom, conn.intern_atom(false, atom.as_ref().as_bytes())?)))
            .collect::<Result<Vec<_>>>()?;

        // ..then get all the replies (so that we only need one instead of many round-trips to the
        // X11 server)
        let atoms = atom_requests
            .into_iter()
            .map(|(atom, cookie)| Ok((atom, cookie.reply()?.atom)))
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(Self { atoms })
    }

    pub(crate) fn known_atom(&self, atom: Atom) -> u32 {
        *self.atoms.get(&atom).unwrap()
    }

    pub(crate) fn atom_name(&self, atom: u32) -> Option<Atom> {
        self.atoms
            .iter()
            .find(|(_, value)| atom == **value)
            .map(|(key, _)| *key)
    }
}

/// Handles communication with an X server via the x11rb crate.
#[derive(Debug)]
pub struct Conn<C: Connection> {
    conn: C,
    root: u32,
    atoms: Atoms,
}

/// A pure rust based connection to the X server using a [RustConnection].
pub type RustConn = Conn<RustConnection>;

impl Conn<RustConnection> {
    /// Construct an X11rbConnection  backed by the [x11rb][crate::x11rb] backend using
    /// [x11rb::rust_connection::RustConnection].
    pub fn new() -> Result<Self> {
        let (conn, _) = RustConnection::connect(None).map_err(Error::from)?;

        Self::new_for_connection(conn)
    }
}

/// An C based connection to the X server using an [XCBConnection].
pub type XcbConn = Conn<XCBConnection>;

impl Conn<XCBConnection> {
    /// Construct an X11rbConnection  backed by the [x11rb][crate::x11rb] backend using
    /// [x11rb::xcb_ffi::XCBConnection].
    pub fn new() -> Result<Self> {
        let (conn, _) = XCBConnection::connect(None).map_err(Error::from)?;

        Self::new_for_connection(conn)
    }
}

impl<C> Conn<C>
where
    C: Connection,
{
    fn new_for_connection(conn: C) -> Result<Self> {
        let root = conn.setup().roots[0].root;
        conn.prefetch_extension_information(randr::X11_EXTENSION_NAME)?;
        let atoms = Atoms::new(&conn)?;

        let extension_info = conn.extension_information(randr::X11_EXTENSION_NAME)?;
        if extension_info.is_none() {
            return Err(Error::Randr("RandR not supported".to_string()));
        }

        let randr_ver = conn
            .randr_query_version(RANDR_VER.0, RANDR_VER.1)?
            .reply()?;
        let (maj, min) = (randr_ver.major_version, randr_ver.minor_version);
        if (maj, min) != RANDR_VER {
            return Err(Error::Randr(format!(
                "penrose requires RandR version >= {}.{}: detected {}.{}\nplease update RandR to a newer version",
                RANDR_VER.0, RANDR_VER.1, maj, min
            )));
        }

        let mask = NotifyMask::OUTPUT_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::SCREEN_CHANGE;
        conn.randr_select_input(root, mask)?;

        let xconn = Self { conn, root, atoms };

        xconn.set_client_attributes(Xid(root), &[ClientAttr::RootEventMask])?;

        Ok(xconn)
    }

    /// Get a handle to the underlying connection.
    pub fn connection(&self) -> &C {
        &self.conn
    }

    /// Create and map a new window to the screen with the specified [WinType].
    pub fn create_window(&self, ty: WinType, r: Rect, managed: bool) -> Result<Xid> {
        let (ty, mut win_aux, class) = match ty {
            WinType::CheckWin => (None, CreateWindowAux::new(), WindowClass::INPUT_OUTPUT),

            WinType::InputOnly => (None, CreateWindowAux::new(), WindowClass::INPUT_ONLY),

            WinType::InputOutput(a) => {
                let colormap = self.conn.generate_id()?;
                let screen = &self.conn.setup().roots[0];

                self.conn.create_colormap(
                    ColormapAlloc::NONE,
                    colormap,
                    screen.root,
                    screen.root_visual,
                )?;

                let win_aux = CreateWindowAux::new()
                    .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY)
                    .background_pixel(x11rb::NONE)
                    .border_pixel(screen.black_pixel)
                    .colormap(colormap);

                (Some(a), win_aux, WindowClass::INPUT_OUTPUT)
            }
        };

        if !managed {
            win_aux = win_aux.override_redirect(1);
        }

        let Rect { x, y, w, h } = r;
        let id = Xid(self.conn.generate_id()?);
        let border_width = 0;

        self.conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            *id,
            self.root,
            x as i16,
            y as i16,
            w as u16,
            h as u16,
            border_width,
            class,
            x11rb::COPY_FROM_PARENT,
            &win_aux,
        )?;

        // Input only windows don't need mapping
        if let Some(atom) = ty {
            let net_name = Atom::NetWmWindowType.as_ref();
            self.set_prop(id, net_name, Prop::Atom(vec![atom.as_ref().into()]))?;
            self.map(id)?;
        }

        self.flush();

        Ok(id)
    }
}

impl<C> XConn for Conn<C>
where
    C: Connection,
{
    fn root(&self) -> Xid {
        self.root.into()
    }

    fn screen_details(&self) -> Result<Vec<Rect>> {
        let resources = self.conn.randr_get_screen_resources(self.root)?.reply()?;

        // Send queries for all CRTCs
        let crtcs = resources
            .crtcs
            .iter()
            .map(|c| {
                self.conn
                    .randr_get_crtc_info(*c, 0)
                    .map_err(|err| err.into())
            })
            .collect::<Result<Vec<_>>>()?;

        let rects = crtcs
            .into_iter()
            .flat_map(|cookie| cookie.reply().ok())
            .filter(|reply| reply.width > 0)
            .map(|reply| {
                Rect::new(
                    reply.x as u32,
                    reply.y as u32,
                    reply.width as u32,
                    reply.height as u32,
                )
            })
            .collect();

        Ok(rects)
    }

    fn cursor_position(&self) -> Result<Point> {
        let reply = self.conn.query_pointer(self.root)?.reply()?;

        Ok(Point::new(reply.root_x as u32, reply.root_y as u32))
    }

    fn grab(&self, key_codes: &[KeyCode], mouse_states: &[MouseState]) -> Result<()> {
        // We need to explicitly grab NumLock as an additional modifier and then drop it later on
        // when we are passing events through to the WindowManager as NumLock alters the modifier
        // mask when it is active.
        let modifiers = &[0, u16::from(ModMask::M2)];
        let mode = GrabMode::ASYNC;
        let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::BUTTON_MOTION;
        let mask = u16::try_from(u32::from(mask)).unwrap();

        for m in modifiers.iter() {
            for k in key_codes.iter() {
                self.conn.grab_key(
                    false,      // don't pass grabbed events through to the client
                    self.root,  // the window to grab: in this case the root window
                    k.mask | m, // modifiers to grab
                    k.code,     // keycode to grab
                    mode,       // don't lock pointer input while grabbing
                    mode,       // don't lock keyboard input while grabbing
                )?;
            }
        }

        for m in modifiers.iter() {
            for state in mouse_states.iter() {
                let button = state.button().into();
                self.conn.grab_button(
                    false,            // don't pass grabbed events through to the client
                    self.root,        // the window to grab: in this case the root window
                    mask,             // which events are reported to the client
                    mode,             // don't lock pointer input while grabbing
                    mode,             // don't lock keyboard input while grabbing
                    x11rb::NONE,      // don't confine the cursor to a specific window
                    x11rb::NONE,      // don't change the cursor type
                    button,           // the button to grab
                    state.mask() | m, // modifiers to grab
                )?;
            }
        }

        self.flush();

        Ok(())
    }

    fn next_event(&self) -> Result<XEvent> {
        loop {
            let event = self.conn.wait_for_event()?;
            if let Some(event) = convert_event(self, event)? {
                return Ok(event);
            }
        }
    }

    fn flush(&self) {
        self.conn.flush().unwrap_or(());
    }

    fn intern_atom(&self, atom: &str) -> Result<Xid> {
        let id = match Atom::from_str(atom) {
            Ok(known) => self.atoms.known_atom(known),
            Err(_) => self.conn.intern_atom(false, atom.as_bytes())?.reply()?.atom,
        };

        Ok(Xid(id))
    }

    fn atom_name(&self, xid: Xid) -> Result<String> {
        // Is the atom already known?
        if let Some(atom) = self.atoms.atom_name(*xid) {
            return Ok(atom.as_ref().to_string());
        }

        // Nope, ask the X11 server
        let reply = self.conn.get_atom_name(*xid)?.reply()?;
        let name = String::from_utf8(reply.name).map_err(Error::from)?;

        Ok(name)
    }

    fn client_geometry(&self, id: Xid) -> Result<Rect> {
        let res = self.conn.get_geometry(*id)?.reply()?;

        Ok(Rect::new(
            res.x as u32,
            res.y as u32,
            res.width as u32,
            res.height as u32,
        ))
    }

    fn existing_clients(&self) -> Result<Vec<Xid>> {
        let raw_ids = self.conn.query_tree(self.root)?.reply()?.children;
        let ids = raw_ids.into_iter().map(Xid).collect();

        Ok(ids)
    }

    fn map(&self, client: Xid) -> Result<()> {
        self.conn.map_window(*client)?;

        Ok(())
    }

    fn unmap(&self, client: Xid) -> Result<()> {
        self.conn.unmap_window(*client)?;

        Ok(())
    }

    fn kill(&self, client: Xid) -> Result<()> {
        self.conn.kill_client(*client)?;

        Ok(())
    }

    fn focus(&self, id: Xid) -> Result<()> {
        self.conn
            .set_input_focus(InputFocus::PARENT, *id, CURRENT_TIME)?;

        Ok(())
    }

    fn get_prop(&self, id: Xid, prop_name: &str) -> Result<Option<Prop>> {
        let atom = *self.intern_atom(prop_name)?;
        let r = self
            .conn
            .get_property(false, *id, atom, AtomEnum::ANY, 0, 1024)?
            .reply()?;

        let prop_type = match r.type_ {
            0 => return Ok(None), // Null response
            id => self.atom_name(Xid(id))?,
        };

        let p = match prop_type.as_ref() {
            "ATOM" => Prop::Atom(
                r.value32()
                    .ok_or_else(|| Error::InvalidPropertyData {
                        id,
                        prop: prop_name.to_owned(),
                        ty: prop_type.to_owned(),
                    })?
                    .map(|a| self.atom_name(Xid(a)))
                    .collect::<Result<Vec<String>>>()?,
            ),

            "CARDINAL" => Prop::Cardinal(
                r.value32()
                    .ok_or_else(|| Error::InvalidPropertyData {
                        id,
                        prop: prop_name.to_owned(),
                        ty: prop_type.to_owned(),
                    })?
                    .collect(),
            ),

            "STRING" | "UTF8_STRING" => {
                if r.format != 8 {
                    return Err(Error::InvalidPropertyData {
                        id,
                        prop: prop_name.to_owned(),
                        ty: prop_type.to_owned(),
                    });
                } else {
                    Prop::UTF8String(
                        String::from_utf8(r.value)?
                            .trim_matches('\0')
                            .split('\0')
                            .map(|s| s.to_string())
                            .collect(),
                    )
                }
            }

            "WINDOW" => Prop::Window(
                r.value32()
                    .ok_or_else(|| Error::InvalidPropertyData {
                        id,
                        prop: prop_name.to_owned(),
                        ty: prop_type.to_owned(),
                    })?
                    .map(Xid)
                    .collect(),
            ),

            "WM_HINTS" => Prop::WmHints(WmHints::try_from_bytes(
                &r.value32()
                    .ok_or_else(|| Error::InvalidPropertyData {
                        id,
                        prop: prop_name.to_owned(),
                        ty: prop_type.to_owned(),
                    })?
                    .collect::<Vec<_>>(),
            )?),

            "WM_SIZE_HINTS" => Prop::WmNormalHints(WmNormalHints::try_from_bytes(
                &r.value32()
                    .ok_or_else(|| Error::InvalidPropertyData {
                        id,
                        prop: prop_name.to_owned(),
                        ty: prop_type.to_owned(),
                    })?
                    .collect::<Vec<_>>(),
            )?),

            // Default to returning the raw bytes as u32s which the user can then
            // convert as needed if the prop type is not one we recognise
            _ => Prop::Bytes(match r.format {
                8 => r.value8().unwrap().map(From::from).collect(),
                16 => r.value16().unwrap().map(From::from).collect(),
                32 => r.value32().unwrap().collect(),
                _ => {
                    error!(
                        "prop type for {} was {} which claims to have a data format of {}",
                        prop_name, prop_type, r.type_
                    );

                    return Ok(None);
                }
            }),
        };

        Ok(Some(p))
    }

    fn get_window_attributes(&self, id: Xid) -> Result<WindowAttributes> {
        let win_attrs = self.conn.get_window_attributes(*id)?.reply()?;

        let map_state = match win_attrs.map_state {
            MapState::UNMAPPED => x::property::MapState::Unmapped,
            MapState::UNVIEWABLE => x::property::MapState::UnViewable,
            MapState::VIEWABLE => x::property::MapState::Viewable,
            s => panic!("got invalid map state from x server: {s:?}"),
        };

        let window_class = match win_attrs.class {
            WindowClass::COPY_FROM_PARENT => x::property::WindowClass::CopyFromParent,
            WindowClass::INPUT_OUTPUT => x::property::WindowClass::InputOutput,
            WindowClass::INPUT_ONLY => x::property::WindowClass::InputOnly,
            c => panic!("got invalid window class from x server: {c:?}"),
        };

        Ok(WindowAttributes::new(
            win_attrs.override_redirect,
            map_state,
            window_class,
        ))
    }

    fn set_wm_state(&self, id: Xid, wm_state: WmState) -> Result<()> {
        let mode = PropMode::REPLACE;
        let a = *self.intern_atom(Atom::WmState.as_ref())?;
        let state = match wm_state {
            WmState::Withdrawn => 0,
            WmState::Normal => 1,
            WmState::Iconic => 3,
        };

        self.conn.change_property32(mode, *id, a, a, &[state])?;

        Ok(())
    }

    fn set_prop(&self, id: Xid, name: &str, val: Prop) -> Result<()> {
        let a = *self.intern_atom(name)?;

        let (ty, data) = match val {
            Prop::UTF8String(strs) => {
                self.conn.change_property8(
                    PropMode::REPLACE,
                    *id,
                    a,
                    AtomEnum::STRING,
                    strs.join("\0").as_bytes(),
                )?;

                return Ok(());
            }

            Prop::Atom(atoms) => (
                AtomEnum::ATOM,
                atoms
                    .iter()
                    .map(|a| self.intern_atom(a).map(|id| *id))
                    .collect::<Result<Vec<u32>>>()?,
            ),

            Prop::Cardinal(vals) => (AtomEnum::CARDINAL, vals),

            Prop::Window(ids) => (AtomEnum::WINDOW, ids.into_iter().map(|id| *id).collect()),

            // FIXME: handle changing WmHints and WmNormalHints correctly in change_prop
            Prop::Bytes(_) | Prop::WmHints(_) | Prop::WmNormalHints(_) => {
                panic!("unable to change Prop, WmHints or WmNormalHints properties");
            }
        };

        self.conn
            .change_property32(PropMode::REPLACE, *id, a, ty, &data)?;

        Ok(())
    }

    fn set_client_attributes(&self, id: Xid, attrs: &[ClientAttr]) -> Result<()> {
        let client_event_mask = EventMask::ENTER_WINDOW
            | EventMask::LEAVE_WINDOW
            | EventMask::PROPERTY_CHANGE
            | EventMask::STRUCTURE_NOTIFY;

        let client_unmap_mask =
            EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW | EventMask::PROPERTY_CHANGE;

        let root_event_mask = EventMask::PROPERTY_CHANGE
            | EventMask::SUBSTRUCTURE_REDIRECT
            | EventMask::SUBSTRUCTURE_NOTIFY
            | EventMask::BUTTON_MOTION;

        let mut aux = ChangeWindowAttributesAux::new();
        for conf in attrs.iter() {
            match conf {
                ClientAttr::BorderColor(c) => aux = aux.border_pixel(*c),
                ClientAttr::ClientEventMask => aux = aux.event_mask(client_event_mask),
                ClientAttr::ClientUnmapMask => aux = aux.event_mask(client_unmap_mask),
                ClientAttr::RootEventMask => aux = aux.event_mask(root_event_mask),
            }
        }
        self.conn.change_window_attributes(*id, &aux)?;

        Ok(())
    }

    fn set_client_config(&self, id: Xid, data: &[ClientConfig]) -> Result<()> {
        let mut aux = ConfigureWindowAux::new();
        for conf in data.iter() {
            match conf {
                ClientConfig::BorderPx(px) => aux = aux.border_width(*px),
                ClientConfig::Position(r) => {
                    aux = aux.x(r.x as i32).y(r.y as i32).width(r.w).height(r.h);
                }
                ClientConfig::StackBelow(s) => aux = aux.sibling(s.0).stack_mode(StackMode::BELOW),
                ClientConfig::StackAbove(s) => aux = aux.sibling(s.0).stack_mode(StackMode::ABOVE),
                ClientConfig::StackBottom => aux = aux.stack_mode(StackMode::BELOW),
                ClientConfig::StackTop => aux = aux.stack_mode(StackMode::ABOVE),
            }
        }
        self.conn.configure_window(*id, &aux)?;

        Ok(())
    }

    fn send_client_message(&self, msg: ClientMessage) -> Result<()> {
        let type_ = *self.intern_atom(&msg.dtype)?;
        let data = match msg.data {
            x::event::ClientMessageData::U8(u8s) => ClientMessageData::from(u8s),
            x::event::ClientMessageData::U16(u16s) => ClientMessageData::from(u16s),
            x::event::ClientMessageData::U32(u32s) => ClientMessageData::from(u32s),
        };
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: *msg.id,
            type_,
            data,
        };
        let mask = match msg.mask {
            ClientEventMask::NoEventMask => EventMask::NO_EVENT,
            ClientEventMask::StructureNotify => EventMask::STRUCTURE_NOTIFY,
            ClientEventMask::SubstructureNotify => EventMask::SUBSTRUCTURE_NOTIFY,
        };

        self.conn.send_event(false, *msg.id, mask, event)?;

        Ok(())
    }

    fn warp_pointer(&self, id: Xid, x: i16, y: i16) -> Result<()> {
        self.conn.warp_pointer(x11rb::NONE, *id, 0, 0, 0, 0, x, y)?;

        Ok(())
    }
}
