//! Builds GUI -> service command frames from the mapped command set.
//!
//! A command carries the request id `034a` first, then the command code `0300`,
//! then its arguments (loose attributes or an argument container, per command).
//! A request id of `NO_REPLY` (`0xffffffff`) tells the service not to answer;
//! the GUI uses it only for the periodic visible-nodes ping. See
//! `docs/PROTOCOL.md` for the command table.
//!
//! Commands that need the `030d` password blob (CreateNetwork, JoinNetwork) are
//! intentionally absent until the blob cipher is recovered by static analysis.

use crate::proto::{Attr, Value};

/// Request id that suppresses the service reply.
pub const NO_REPLY: u32 = 0xffff_ffff;

// Command codes (attribute 0300). Separate number space from event types.
pub mod cmd {
    pub const SWITCH_POWER: u32 = 4;
    pub const LEAVE_NETWORK: u32 = 5;
    pub const UPDATE_NETWORK: u32 = 14;
    pub const DELETE_NETWORK: u32 = 15;
    pub const CHANGE_ALIAS: u32 = 18;
    pub const UPDATE_VISIBLE_NODES: u32 = 20;
    pub const LANGUAGE_CHANGED: u32 = 28;
}

// Attribute tags used when building commands.
mod tag {
    pub const REQ_ID: u16 = 0x034a;
    pub const CMD: u16 = 0x0300;
    pub const ALIAS: u16 = 0x0304;
    pub const NET_NAME: u16 = 0x0306;
    pub const NET_PROP_A: u16 = 0x0308;
    pub const NET_GUID: u16 = 0x0309;
    pub const POWER: u16 = 0x0311;
    pub const NET_PROP_B: u16 = 0x031c;
    pub const NET_PROP_C: u16 = 0x0323;
    pub const LANG: u16 = 0x025a;
    pub const VISIBLE_PARAM: u16 = 0x0376;
    pub const NODE_ID: u16 = 0x01e1;
    pub const UPDATE_NET_ARGS: u16 = 0x132a;
    pub const VISIBLE_ARGS: u16 = 0x1344;
}

/// Starts a command: request id then command code. Args are appended by caller.
fn head(req_id: u32, code: u32) -> Vec<Attr> {
    vec![Attr::u32(tag::REQ_ID, req_id), Attr::u32(tag::CMD, code)]
}

/// Turns the VPN on or off (command 4).
pub fn switch_power(req_id: u32, on: bool) -> Vec<Attr> {
    let mut m = head(req_id, cmd::SWITCH_POWER);
    m.push(Attr::bool(tag::POWER, on));
    m
}

/// Leaves a network (command 5).
pub fn leave_network(req_id: u32, net: [u8; 16]) -> Vec<Attr> {
    let mut m = head(req_id, cmd::LEAVE_NETWORK);
    m.push(Attr::guid(tag::NET_GUID, net));
    m
}

/// Deletes a network the user owns (command 15).
pub fn delete_network(req_id: u32, net: [u8; 16]) -> Vec<Attr> {
    let mut m = head(req_id, cmd::DELETE_NETWORK);
    m.push(Attr::guid(tag::NET_GUID, net));
    m
}

/// Renames this node (command 18).
pub fn change_alias(req_id: u32, alias: &str) -> Vec<Attr> {
    let mut m = head(req_id, cmd::CHANGE_ALIAS);
    m.push(Attr::str(tag::ALIAS, alias));
    m
}

/// Updates a network's name and boolean properties (command 14). The three
/// booleans match the create flags; their exact meaning is still unmapped, so
/// callers pass through whatever the current state reports.
pub fn update_network(
    req_id: u32,
    net: [u8; 16],
    name: &str,
    prop_a: bool,
    prop_b: bool,
    prop_c: bool,
) -> Vec<Attr> {
    let mut m = head(req_id, cmd::UPDATE_NETWORK);
    m.push(Attr::container(
        tag::UPDATE_NET_ARGS,
        vec![
            Attr::guid(tag::NET_GUID, net),
            Attr::str(tag::NET_NAME, name),
            Attr::bool(tag::NET_PROP_A, prop_a),
            Attr::bool(tag::NET_PROP_C, prop_c),
            Attr::bool(tag::NET_PROP_B, prop_b),
        ],
    ));
    m
}

/// Tells the service which nodes the window is showing (command 20). Sent every
/// ~3 s with `NO_REPLY`, mirroring the original GUI's heartbeat.
pub fn update_visible_nodes(nodes: &[u64]) -> Vec<Attr> {
    let mut m = head(NO_REPLY, cmd::UPDATE_VISIBLE_NODES);
    let args: Vec<Attr> = nodes
        .iter()
        .flat_map(|&id| {
            [
                Attr::new(tag::NODE_ID, Value::U64(id)),
                Attr::u32(tag::VISIBLE_PARAM, 0),
            ]
        })
        .collect();
    m.push(Attr::container(tag::VISIBLE_ARGS, args));
    m
}

/// Announces the UI language (command 28). `ids` are Windows LCIDs (1033 en-US,
/// 1046 pt-BR).
pub fn language_changed(req_id: u32, ids: [u32; 2]) -> Vec<Attr> {
    let mut m = head(req_id, cmd::LANGUAGE_CHANGED);
    m.push(Attr::u32(tag::LANG, ids[0]));
    m.push(Attr::u32(tag::LANG, ids[1]));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{decode_body, encode_frame, FrameReader, Value};

    fn first(attrs: &[Attr], tag: u16) -> &Value {
        &attrs.iter().find(|a| a.tag == tag).unwrap().value
    }

    #[test]
    fn switch_power_shape() {
        let m = switch_power(7, true);
        assert_eq!(first(&m, 0x034a), &Value::U32(7));
        assert_eq!(first(&m, 0x0300), &Value::U32(cmd::SWITCH_POWER));
        assert_eq!(first(&m, 0x0311), &Value::Bool(true));
    }

    #[test]
    fn leave_carries_guid() {
        let g = [0xab; 16];
        let m = leave_network(1, g);
        assert_eq!(first(&m, 0x0309), &Value::Guid(g));
    }

    #[test]
    fn visible_nodes_is_no_reply() {
        let m = update_visible_nodes(&[10, 20]);
        assert_eq!(first(&m, 0x034a), &Value::U32(NO_REPLY));
        let Value::Container(args) = first(&m, 0x1344) else {
            panic!("expected container");
        };
        // Two node ids -> two (id, param) pairs -> four attributes.
        assert_eq!(args.len(), 4);
    }

    #[test]
    fn command_survives_frame_roundtrip() {
        let m = change_alias(3, "nodo-teste");
        let frame = encode_frame(&m);
        let mut r = FrameReader::new();
        r.feed(&frame);
        let body = r.next_frame().unwrap();
        assert_eq!(decode_body(&body).unwrap(), m);
    }
}
