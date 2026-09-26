// SPDX-License-Identifier: Apache-2.0
//! The build-time closure check, kept free of any dependency so `build.rs` can
//! compile it as well as the library (whose tests exercise it on small
//! hand-made protocols).
//!
//! A global is safe to forward only if the proxy can count and translate every
//! descriptor any object reachable from it can carry. "Reachable" means: the
//! global's interface, and everything a typed `new_id` in a request *or* an
//! event of a reachable interface creates, to a fixed point. The one untyped
//! `new_id` in the core protocol, `wl_registry.bind`, is exactly the edge the
//! allowlist controls, so it is not followed.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap};

/// One message, reduced to what the check needs.
#[derive(Clone, Debug, Default)]
pub struct ModelMsg {
    pub name: String,
    pub nfds: usize,
    /// Interfaces this message creates through typed `new_id` arguments.
    pub creates: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ModelIface {
    pub name: String,
    pub requests: Vec<ModelMsg>,
    pub events: Vec<ModelMsg>,
}

/// Everything reachable from `root`, or the first interface referenced but not
/// defined anywhere in `ifaces`.
pub fn reachable(ifaces: &[ModelIface], root: &str) -> Result<BTreeSet<String>, String> {
    let by_name: HashMap<&str, &ModelIface> = ifaces.iter().map(|i| (i.name.as_str(), i)).collect();
    let mut seen = BTreeSet::new();
    let mut todo = vec![(root.to_string(), String::from("(global)"))];
    while let Some((name, via)) = todo.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(iface) = by_name.get(name.as_str()) else {
            return Err(format!(
                "interface {name} (created by {via}) is not in any vendored XML"
            ));
        };
        for m in iface.requests.iter().chain(iface.events.iter()) {
            for c in &m.creates {
                todo.push((c.clone(), format!("{}.{}", iface.name, m.name)));
            }
        }
    }
    Ok(seen)
}

/// Check every global against the descriptor policy. Returns one line per
/// problem; empty means the allowlist is closed.
pub fn check(ifaces: &[ModelIface], globals: &[&str], fd_policy: &[(&str, &str)]) -> Vec<String> {
    let mut errors = Vec::new();
    let by_name: HashMap<&str, &ModelIface> = ifaces.iter().map(|i| (i.name.as_str(), i)).collect();

    // Every policy line must name a real message carrying exactly one fd: a
    // typo would otherwise silently leave the real message unhandled, and the
    // per-message class cannot tell two descriptors apart.
    for (iface, msg) in fd_policy {
        let Some(i) = by_name.get(iface) else {
            errors.push(format!("FD_POLICY names unknown interface {iface}"));
            continue;
        };
        match i
            .requests
            .iter()
            .chain(i.events.iter())
            .find(|m| m.name == *msg)
        {
            None => errors.push(format!("FD_POLICY names unknown message {iface}.{msg}")),
            Some(m) if m.nfds != 1 => errors.push(format!(
                "FD_POLICY entry {iface}.{msg} carries {} descriptors, not 1",
                m.nfds
            )),
            Some(_) => {}
        }
    }

    // wl_display is every connection's root and needs no allowing.
    let mut roots: Vec<&str> = vec!["wl_display"];
    roots.extend_from_slice(globals);
    for root in roots {
        if !by_name.contains_key(root) {
            errors.push(format!("allowed global {root} is not in any vendored XML"));
            continue;
        }
        let set = match reachable(ifaces, root) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("{root}: {e}"));
                continue;
            }
        };
        for name in &set {
            let i = by_name[name.as_str()];
            for m in i.requests.iter().chain(i.events.iter()) {
                if m.nfds > 0
                    && !fd_policy
                        .iter()
                        .any(|(pi, pm)| *pi == i.name && *pm == m.name)
                {
                    errors.push(format!(
                        "global {root} reaches {}.{} which carries a descriptor with no FD_POLICY entry",
                        i.name, m.name
                    ));
                }
            }
        }
    }
    errors
}
