//! Service -> GUI state: networks and their member nodes, built from events.
//!
//! The service sends a full-state event (type 34) after connect, then a stream
//! of deltas (NetworksChanged 7, latency tick 28, connect progress 5, per-node
//! updates). This module turns those event bodies into a flat, UI-ready model.
//!
//! The exact nesting of the full-state tree is only partly pinned down in
//! `docs/PROTOCOL.md`, so parsing is deliberately structural rather than
//! position-based: any container carrying a network GUID + name is read as a
//! network, and any container carrying a node id is read as a node. This stays
//! correct even if the wrapper container tags differ from a given capture, but
//! it still needs validating against a real event-34 capture.

use std::net::Ipv4Addr;

use crate::proto::{Attr, Value};

// Event types (attribute 0301). Separate number space from command codes.
pub mod event {
    pub const FULL_STATE: u32 = 34;
    pub const NETWORKS_CHANGED: u32 = 7;
    pub const CONNECT_PROGRESS: u32 = 5;
    pub const LATENCY_TICK: u32 = 28;
}

// Attribute tags read from events. Tag meanings and nesting were confirmed
// against a live event-34 capture; see `docs/PROTOCOL.md`.
mod tag {
    pub const EVENT: u16 = 0x0301;
    pub const NODE_ID: u16 = 0x01e1;
    pub const ALIAS: u16 = 0x0304; // node/computer name
    pub const ADDR: u16 = 0x0305; // first occurrence is IPv4 (u32); IPv6 repeats the tag
    pub const NET_NAME: u16 = 0x0306;
    pub const NET_PROP_A: u16 = 0x0308; // allow connect without password
    pub const NET_GUID: u16 = 0x0309;
    pub const ROLE: u16 = 0x030a; // 0 applicant, 1 member, 2 administrator
    pub const POWER: u16 = 0x0311; // self power bool
    pub const CONNECT_STATE: u16 = 0x0312;
    pub const NET_PROP_C: u16 = 0x0323; // block joining for new members
    pub const VERSION: u16 = 0x01ed; // "2.1.1"
    pub const RTT: u16 = 0x03a0;
    pub const LATENCY: u16 = 0x034d;
    pub const MEMBER_COUNT: u16 = 0x03a3;
    pub const CHANNEL: u16 = 0x1266; // present when a peer has an active connection
    pub const MEMBER_REF: u16 = 0x1324; // network membership entry (node id + kind only)
}

/// A single peer in a network. Identity comes from the network's membership
/// list; the rest is merged in from the node-detail records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub id: u64,
    pub alias: String,
    pub ipv4: Option<Ipv4Addr>,
    /// Heuristic: the service does not send a plain online flag here, so a node
    /// counts as online when it has an active channel or a measured rtt.
    pub online: bool,
    pub rtt_ms: Option<u32>,
    pub version: Option<String>,
    /// Role in the network this node is listed under (see [`role`]).
    pub role: Option<u32>,
}

/// Network roles carried by tag `0x030a`, matching the original GUI's labels.
pub mod role {
    pub const APPLICANT: u32 = 0; // asked to join, waiting for an admin's approval
    pub const MEMBER: u32 = 1;
    pub const ADMIN: u32 = 2;
}

impl Node {
    /// True when the node asked to join and still waits for approval.
    pub fn is_applicant(&self) -> bool {
        self.role == Some(role::APPLICANT)
    }
}

impl Network {
    /// Whether this machine may delete the network. Unknown role: allow, the
    /// service has the final word.
    pub fn can_delete(&self) -> bool {
        self.self_role.map_or(true, |r| r == role::ADMIN)
    }

    /// The service refuses to let the last administrator leave; it has to
    /// delete the network instead.
    pub fn is_last_admin(&self) -> bool {
        self.self_role == Some(role::ADMIN)
            && !self.nodes.iter().any(|n| n.role == Some(role::ADMIN))
    }
}

/// A network (room) and the peers currently in it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Network {
    pub guid: [u8; 16],
    pub name: String,
    pub members: u32,
    pub allow_without_password: bool,
    pub block_new_members: bool,
    pub nodes: Vec<Node>,
    pub has_prop_a: bool,
    pub has_prop_c: bool,
    /// This machine's own role in the network.
    pub self_role: Option<u32>,
}

/// The whole picture the UI draws.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub networks: Vec<Network>,
    /// This machine's own alias, from the self node.
    pub self_alias: String,
    /// Self power flag from the full-state event, if seen.
    pub power: Option<bool>,
    /// Latest connect-progress state code (event 5 or the self node), if any.
    pub connect_state: Option<u32>,
}

impl State {
    /// Applies one decoded event body, mutating the model. Unknown event types
    /// are ignored. Returns true if anything changed.
    pub fn apply(&mut self, attrs: &[Attr]) -> bool {
        let Some(ev) = find_u32(attrs, tag::EVENT) else {
            return false;
        };
        match ev {
            event::FULL_STATE => {
                let next = parse_full_state(attrs);
                let changed = next != *self;
                *self = next;
                changed
            }
            event::NETWORKS_CHANGED => {
                let mut details = std::collections::HashMap::new();
                collect_node_details(attrs, &mut details);
                let mut incoming = Vec::new();
                collect_networks(attrs, &details, &mut incoming);
                let mut changed = false;
                for n in incoming {
                    changed |= self.upsert_network(n);
                }
                changed
            }
            event::LATENCY_TICK => {
                let (Some(id), Some(ms)) =
                    (find_u64(attrs, tag::NODE_ID), find_u32(attrs, tag::LATENCY))
                else {
                    return false;
                };
                self.set_node_rtt(id, ms)
            }
            event::CONNECT_PROGRESS => {
                let s = find_u32(attrs, tag::CONNECT_STATE);
                let changed = s != self.connect_state;
                self.connect_state = s;
                changed
            }
            _ => false,
        }
    }

    /// Inserts a network or replaces the existing one with the same GUID.
    fn upsert_network(&mut self, mut net: Network) -> bool {
        if let Some(slot) = self.networks.iter_mut().find(|n| n.guid == net.guid) {
            // Se o delta recebido (ex: NETWORKS_CHANGED 0x1329) vier sem lista de nós,
            // preserva os nós já conhecidos.
            if net.nodes.is_empty() && !slot.nodes.is_empty() {
                net.nodes = std::mem::take(&mut slot.nodes);
            }
            // Se o membro count vier zerado no delta, preserva o anterior
            if net.members == 0 && slot.members > 0 {
                net.members = slot.members;
            }
            // Se as propriedades não vierem no novo registro, preserva as anteriores
            if !net.has_prop_a && slot.has_prop_a {
                net.allow_without_password = slot.allow_without_password;
                net.has_prop_a = true;
            }
            if net.self_role.is_none() {
                net.self_role = slot.self_role;
            }
            if !net.has_prop_c && slot.has_prop_c {
                net.block_new_members = slot.block_new_members;
                net.has_prop_c = true;
            }
            if *slot == net {
                return false;
            }
            *slot = net;
        } else {
            self.networks.push(net);
        }
        true
    }

    /// Marks an applicant as a member right after approving it, so the UI does
    /// not keep showing it as pending until the next full state.
    pub fn mark_approved(&mut self, net: [u8; 16], node: u64) -> bool {
        let Some(n) = self
            .networks
            .iter_mut()
            .find(|n| n.guid == net)
            .and_then(|n| n.nodes.iter_mut().find(|x| x.id == node))
        else {
            return false;
        };
        let changed = n.role != Some(role::MEMBER);
        n.role = Some(role::MEMBER);
        changed
    }

    /// Atualiza manualmente as propriedades de uma rede conhecida (ex: após salvar
    /// no modal de propriedades ou a partir do cache de configurações).
    pub fn update_network_props(
        &mut self,
        guid: [u8; 16],
        allow_without_password: bool,
        block_new_members: bool,
    ) -> bool {
        if let Some(net) = self.networks.iter_mut().find(|n| n.guid == guid) {
            if net.allow_without_password == allow_without_password
                && net.block_new_members == block_new_members
                && net.has_prop_a
                && net.has_prop_c
            {
                return false;
            }
            net.allow_without_password = allow_without_password;
            net.block_new_members = block_new_members;
            net.has_prop_a = true;
            net.has_prop_c = true;
            return true;
        }
        false
    }

    fn set_node_rtt(&mut self, node_id: u64, ms: u32) -> bool {
        for net in &mut self.networks {
            if let Some(node) = net.nodes.iter_mut().find(|n| n.id == node_id) {
                let online = ms > 0;
                if node.rtt_ms == Some(ms) && node.online == online {
                    return false;
                }
                node.rtt_ms = Some(ms);
                node.online = online;
                return true;
            }
        }
        false
    }
}

/// Builds the full state from an event-34 body.
///
/// The tree holds two separate views of nodes: detail records (a container with
/// both a node id and an alias, e.g. the self node `0x1320` and each peer
/// `0x1322`) and membership refs inside each network (`0x1324`, carrying only a
/// node id). Networks are matched by GUID + name. The function gathers a node
/// detail map, then fills each network's membership with those details.
fn parse_full_state(attrs: &[Attr]) -> State {
    let mut details = std::collections::HashMap::new();
    collect_node_details(attrs, &mut details);

    let mut networks = Vec::new();
    collect_networks(attrs, &details, &mut networks);

    // Self alias/power/connect-state live on the self node, the one container
    // carrying a power flag.
    let mut self_fields = SelfFields::default();
    find_self(attrs, &mut self_fields);

    State {
        networks,
        self_alias: self_fields.alias,
        power: self_fields.power,
        connect_state: self_fields.connect_state,
    }
}

#[derive(Default)]
struct SelfFields {
    alias: String,
    power: Option<bool>,
    connect_state: Option<u32>,
}

/// Collects node-detail records: any container with both a node id and an alias.
fn collect_node_details(attrs: &[Attr], out: &mut std::collections::HashMap<u64, Node>) {
    for a in attrs {
        let Value::Container(children) = &a.value else {
            continue;
        };
        if has_tag(children, tag::NODE_ID) && has_tag(children, tag::ALIAS) {
            let node = parse_node_detail(children);
            out.entry(node.id).or_insert(node);
        }
        // Keep descending: detail records nest further containers (channels).
        collect_node_details(children, out);
    }
}

fn parse_node_detail(children: &[Attr]) -> Node {
    let mut node = Node::default();
    if let Some(Value::U64(id)) = value_of(children, tag::NODE_ID) {
        node.id = *id;
    }
    node.alias = find_str(children, tag::ALIAS).unwrap_or_default();
    node.rtt_ms = find_u32(children, tag::RTT);
    node.version = find_str(children, tag::VERSION);
    // The first 0x305 is the IPv4 address as a u32; a later 0x305 repeats it as
    // IPv6, which we ignore here.
    if let Some(Value::U32(v)) = value_of(children, tag::ADDR) {
        node.ipv4 = Some(Ipv4Addr::from(*v));
    }
    // Um nó só é considerado online se possuir um canal de comunicação ativo
    // (tag CHANNEL 0x1266) ou uma latência medida maior que zero. Nós offline
    // vêm sem canal e com rtt zerado/ausente.
    node.online = has_tag(children, tag::CHANNEL) || node.rtt_ms.map_or(false, |ms| ms > 0);
    node
}

/// Walks the tree and builds every network (container with GUID + name),
/// resolving each membership id against the node-detail map.
fn collect_networks(
    attrs: &[Attr],
    details: &std::collections::HashMap<u64, Node>,
    out: &mut Vec<Network>,
) {
    for a in attrs {
        let Value::Container(children) = &a.value else {
            continue;
        };
        if has_tag(children, tag::NET_GUID) && has_tag(children, tag::NET_NAME) {
            out.push(parse_network(children, details));
        } else {
            collect_networks(children, details, out);
        }
    }
}

fn parse_network(children: &[Attr], details: &std::collections::HashMap<u64, Node>) -> Network {
    let mut net = Network::default();
    if let Some(Value::Guid(g)) = value_of(children, tag::NET_GUID) {
        net.guid = *g;
    }
    net.name = find_str(children, tag::NET_NAME).unwrap_or_default();
    net.members = find_u32(children, tag::MEMBER_COUNT).unwrap_or(0);
    net.has_prop_a = has_tag(children, tag::NET_PROP_A);
    net.allow_without_password = find_bool(children, tag::NET_PROP_A).unwrap_or(false);
    net.has_prop_c = has_tag(children, tag::NET_PROP_C);
    net.block_new_members = find_bool(children, tag::NET_PROP_C).unwrap_or(false);
    net.self_role = find_u32(children, tag::ROLE);
    for a in children {
        if a.tag != tag::MEMBER_REF {
            continue;
        }
        let Value::Container(ref_attrs) = &a.value else {
            continue;
        };
        if let Some(Value::U64(id)) = value_of(ref_attrs, tag::NODE_ID) {
            let mut node = details.get(id).cloned().unwrap_or(Node {
                id: *id,
                ..Node::default()
            });
            node.role = find_u32(ref_attrs, tag::ROLE);
            net.nodes.push(node);
        }
    }
    net
}

/// Finds the self node — the one container carrying a power flag — and reads its
/// alias, power and connect state.
fn find_self(attrs: &[Attr], out: &mut SelfFields) {
    for a in attrs {
        let Value::Container(children) = &a.value else {
            continue;
        };
        if has_tag(children, tag::POWER) {
            out.alias = find_str(children, tag::ALIAS).unwrap_or_default();
            if let Some(Value::Bool(b)) = value_of(children, tag::POWER) {
                out.power = Some(*b);
            }
            out.connect_state = find_u32(children, tag::CONNECT_STATE);
            return;
        }
        find_self(children, out);
    }
}

// -- small readers over a flat attribute list --------------------------------

fn value_of<'a>(attrs: &'a [Attr], tag: u16) -> Option<&'a Value> {
    attrs.iter().find(|a| a.tag == tag).map(|a| &a.value)
}

fn has_tag(attrs: &[Attr], tag: u16) -> bool {
    attrs.iter().any(|a| a.tag == tag)
}

fn find_u32(attrs: &[Attr], tag: u16) -> Option<u32> {
    match value_of(attrs, tag) {
        Some(Value::U32(v)) => Some(*v),
        _ => None,
    }
}

fn find_u64(attrs: &[Attr], tag: u16) -> Option<u64> {
    match value_of(attrs, tag) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    }
}

fn find_str(attrs: &[Attr], tag: u16) -> Option<String> {
    match value_of(attrs, tag) {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn find_bool(attrs: &[Attr], tag: u16) -> Option<bool> {
    match value_of(attrs, tag) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the real event-34 layout: a self node (0x1320) holding a networks
    // root (0x1321) with a peer detail record (0x1322) and a network (0x1323)
    // whose membership refs (0x1324) carry only node ids.
    fn full_state_event() -> Vec<Attr> {
        let peer_detail = Attr::container(
            0x1322,
            vec![
                Attr::new(tag::NODE_ID, Value::U64(10)),
                Attr::str(tag::ALIAS, "DESKTOP-EXAMPLE"),
                Attr::str(tag::VERSION, "2.1.1"),
                Attr::new(tag::ADDR, Value::U32(u32::from(Ipv4Addr::new(26, 1, 2, 3)))),
                Attr::u32(tag::RTT, 75),
                Attr::container(tag::CHANNEL, vec![]),
            ],
        );
        let network = Attr::container(
            0x1323,
            vec![
                Attr::guid(tag::NET_GUID, [0x33; 16]),
                Attr::u32(0x030a, 2),
                Attr::u32(tag::MEMBER_COUNT, 1),
                Attr::str(tag::NET_NAME, "Volakia"),
                Attr::bool(tag::NET_PROP_A, true),
                Attr::bool(tag::NET_PROP_C, false),
                member_ref(1),
                member_ref(10),
            ],
        );
        let networks_root = Attr::container(0x1321, vec![peer_detail, network]);
        let self_node = Attr::container(
            0x1320,
            vec![
                Attr::new(tag::NODE_ID, Value::U64(1)),
                Attr::str(tag::ALIAS, "Val47 - CachyOS"),
                Attr::bool(tag::POWER, true),
                Attr::u32(tag::CONNECT_STATE, 3),
                networks_root,
            ],
        );
        vec![Attr::u32(tag::EVENT, event::FULL_STATE), self_node]
    }

    fn member_ref(id: u64) -> Attr {
        Attr::container(
            tag::MEMBER_REF,
            vec![Attr::new(tag::NODE_ID, Value::U64(id)), Attr::u32(0x030a, 1)],
        )
    }

    #[test]
    fn parses_full_state() {
        let mut st = State::default();
        assert!(st.apply(&full_state_event()));
        assert_eq!(st.self_alias, "Val47 - CachyOS");
        assert_eq!(st.power, Some(true));
        assert_eq!(st.connect_state, Some(3));
        assert_eq!(st.networks.len(), 1);
        let net = &st.networks[0];
        assert_eq!(net.name, "Volakia");
        assert_eq!(net.members, 1);
        assert_eq!(net.allow_without_password, true);
        assert_eq!(net.block_new_members, false);
        assert_eq!(net.nodes.len(), 2);
        // Self member: id only, no detail record under 0x1322.
        assert_eq!(net.nodes[0].id, 1);
        assert_eq!(net.nodes[0].alias, "Val47 - CachyOS");
        // Peer member: detail merged in from 0x1322.
        let peer = &net.nodes[1];
        assert_eq!(peer.id, 10);
        assert_eq!(peer.alias, "DESKTOP-EXAMPLE");
        assert_eq!(peer.ipv4, Some(Ipv4Addr::new(26, 1, 2, 3)));
        assert_eq!(peer.version.as_deref(), Some("2.1.1"));
        assert_eq!(peer.rtt_ms, Some(75));
        assert!(peer.online);
        assert_eq!(peer.role, Some(1));
        assert!(!peer.is_applicant());
        assert_eq!(net.self_role, Some(2));
        assert!(net.can_delete());
        assert!(net.is_last_admin());
    }

    #[test]
    fn member_role_zero_is_applicant() {
        let mut ev = full_state_event();
        // Turn the peer's membership ref (id 10) into an applicant.
        fn patch(attrs: &mut [Attr]) {
            for a in attrs {
                if let Value::Container(ch) = &mut a.value {
                    if a.tag == tag::MEMBER_REF
                        && matches!(value_of(ch, tag::NODE_ID), Some(Value::U64(10)))
                    {
                        ch.retain(|c| c.tag != tag::ROLE);
                        ch.push(Attr::u32(tag::ROLE, role::APPLICANT));
                    }
                    patch(ch);
                }
            }
        }
        patch(&mut ev);
        let mut st = State::default();
        st.apply(&ev);
        assert!(st.networks[0].nodes[1].is_applicant());
    }

    #[test]
    fn latency_tick_updates_rtt() {
        let mut st = State::default();
        st.apply(&full_state_event());
        let tick = vec![
            Attr::u32(tag::EVENT, event::LATENCY_TICK),
            Attr::new(tag::NODE_ID, Value::U64(10)),
            Attr::u32(tag::LATENCY, 120),
        ];
        assert!(st.apply(&tick));
        assert_eq!(st.networks[0].nodes[1].rtt_ms, Some(120));
        // Same value again is a no-op.
        assert!(!st.apply(&tick));
    }

    #[test]
    fn full_state_reapply_without_change_is_noop() {
        let mut st = State::default();
        assert!(st.apply(&full_state_event()));
        assert!(!st.apply(&full_state_event()));
    }

    #[test]
    fn networks_changed_delta_preserves_existing_nodes_and_props() {
        let mut st = State::default();
        assert!(st.apply(&full_state_event()));
        assert_eq!(st.networks[0].nodes.len(), 2);
        assert_eq!(st.networks[0].allow_without_password, true);

        // Simula evento 7 NETWORKS_CHANGED com atributos da rede atualizados mas SEM MEMBER_REF
        let net_delta = Attr::container(
            0x1329,
            vec![
                Attr::guid(tag::NET_GUID, [0x33; 16]),
                Attr::str(tag::NET_NAME, "Volakia Renomeada"),
            ],
        );
        let delta_event = vec![
            Attr::u32(tag::EVENT, event::NETWORKS_CHANGED),
            Attr::container(0x131f, vec![net_delta]),
        ];

        assert!(st.apply(&delta_event));
        // O nome atualizou
        assert_eq!(st.networks[0].name, "Volakia Renomeada");
        // Os nós existentes e as propriedades foram preservados
        assert_eq!(st.networks[0].nodes.len(), 2);
        assert_eq!(st.networks[0].allow_without_password, true);
    }
}
