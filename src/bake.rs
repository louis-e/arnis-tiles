//! Turns one .osm.pbf extract into per-tile payloads, written to the chunk store.
//!
//! Three streaming passes, so peak memory is one node-coordinate array rather than the whole
//! extract: relations first (to learn which untagged ways are multipolygon members), then
//! node coordinates, then ways. Nothing is held that a later pass can re-read.

use crate::format::{self, Node, Relation, Tile, Way};
use crate::store::ChunkStore;
use crate::tags;
use crate::tilemath;
use osmpbf::{Element, ElementReader, RelMemberType};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Degrees -> stored units, at format::COORD_SCALE.
fn q(v: f64) -> i32 {
    (v * format::COORD_SCALE).round() as i32
}

fn deg(v: i32) -> f64 {
    v as f64 / format::COORD_SCALE
}

pub struct Stats {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    pub tiles: usize,
    pub bytes: u64,
}

pub fn bake(pbf: &Path, store: &mut ChunkStore, region: &str, zoom: u8) -> Result<Stats, String> {
    let open = || ElementReader::from_path(pbf).map_err(|e| format!("{}: {e}", pbf.display()));

    // ── pass 0: relations ────────────────────────────────────────────────────
    let mut relations: Vec<Relation> = Vec::new();
    let mut member_ways: HashSet<i64> = HashSet::new();
    open()?
        .for_each(|el| {
            if let Element::Relation(r) = el {
                let t: Vec<(String, String)> = r
                    .tags()
                    .filter(|(k, _)| tags::keep_tag(k))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                if !tags::relation_is_wanted(&t) {
                    return;
                }
                let members: Vec<(u64, String)> = r
                    .members()
                    .filter(|m| m.member_type == RelMemberType::Way)
                    .map(|m| (m.member_id as u64, m.role().unwrap_or("").to_string()))
                    .collect();
                if members.is_empty() {
                    return;
                }
                for (id, _) in &members {
                    member_ways.insert(*id as i64);
                }
                relations.push(Relation {
                    id: r.id() as u64,
                    tags: t,
                    members,
                });
            }
        })
        .map_err(|e| e.to_string())?;

    // ── pass 1: node coordinates (+ the tagged ones, which are POIs in their own right) ──
    let mut coords: Vec<(i64, i32, i32)> = Vec::new();
    let mut pois: Vec<Node> = Vec::new();
    let mut handle_node = |id: i64, lat: f64, lon: f64, t: Vec<(String, String)>| {
        coords.push((id, q(lat), q(lon)));
        if tags::node_is_wanted(&t) {
            pois.push(Node {
                id: id as u64,
                lat: q(lat),
                lon: q(lon),
                tags: t,
            });
        }
    };
    open()?
        .for_each(|el| match el {
            Element::Node(n) => {
                let t = n
                    .tags()
                    .filter(|(k, _)| tags::keep_tag(k))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                handle_node(n.id(), n.lat(), n.lon(), t);
            }
            Element::DenseNode(n) => {
                let t = n
                    .tags()
                    .filter(|(k, _)| tags::keep_tag(k))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                handle_node(n.id(), n.lat(), n.lon(), t);
            }
            _ => {}
        })
        .map_err(|e| e.to_string())?;
    coords.sort_unstable_by_key(|(id, _, _)| *id);
    let lookup = |id: i64| -> Option<(i32, i32)> {
        coords
            .binary_search_by_key(&id, |(i, _, _)| *i)
            .ok()
            .map(|i| (coords[i].1, coords[i].2))
    };

    // ── pass 2: ways, emitted straight into their tiles ──────────────────────
    let mut tiles: HashMap<(u32, u32), Tile> = HashMap::new();
    let mut way_tiles: HashMap<u64, Vec<(u32, u32)>> = HashMap::new();
    let mut n_ways = 0u64;

    open()?
        .for_each(|el| {
            let Element::Way(w) = el else { return };
            let t: Vec<(String, String)> = w
                .tags()
                .filter(|(k, _)| tags::keep_tag(k))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let is_member = member_ways.contains(&w.id());
            if !is_member && !tags::way_is_wanted(&t) {
                return;
            }
            let refs: Vec<i64> = w.refs().collect();
            let raw: Vec<(i32, i32)> = refs.iter().filter_map(|r| lookup(*r)).collect();
            if raw.len() < 2 {
                return;
            }
            // Quantisation can collapse neighbouring vertices; a way that is closed in OSM
            // must still read as closed after it, so closure is recorded from the ids.
            let closed = refs.first() == refs.last() && refs.len() > 2;
            let mut points: Vec<(i32, i32)> = Vec::with_capacity(raw.len());
            for p in raw {
                if points.last() != Some(&p) {
                    points.push(p);
                }
            }
            if closed && points.first() != points.last() {
                if let Some(first) = points.first().copied() {
                    points.push(first);
                }
            }
            if points.len() < 2 {
                return;
            }

            let mut touched: Vec<(u32, u32)> = Vec::new();
            for (lat, lon) in &points {
                let tl = tilemath::tile_of(deg(*lat), deg(*lon), zoom);
                if !touched.contains(&tl) {
                    touched.push(tl);
                }
            }
            let way = Way {
                id: w.id() as u64,
                closed,
                tags: t,
                points,
            };
            for tl in &touched {
                tiles.entry(*tl).or_default().ways.push(way.clone());
            }
            if is_member {
                way_tiles.insert(way.id, touched);
            }
            n_ways += 1;
        })
        .map_err(|e| e.to_string())?;

    // POIs land in their own tile.
    let n_pois = pois.len() as u64;
    for p in pois {
        let tl = tilemath::tile_of(deg(p.lat), deg(p.lon), zoom);
        tiles.entry(tl).or_default().nodes.push(p);
    }

    // A relation goes wherever its members went, so a tile that holds part of a multipolygon
    // also holds the relation that explains it.
    let mut n_rels = 0u64;
    for r in relations {
        let mut where_to: Vec<(u32, u32)> = Vec::new();
        for (wid, _) in &r.members {
            if let Some(ts) = way_tiles.get(wid) {
                for t in ts {
                    if !where_to.contains(t) {
                        where_to.push(*t);
                    }
                }
            }
        }
        if where_to.is_empty() {
            continue;
        }
        n_rels += 1;
        for t in where_to {
            tiles.entry(t).or_default().relations.push(r.clone());
        }
    }

    // ── write ────────────────────────────────────────────────────────────────
    let mut bytes = 0u64;
    let n_tiles = tiles.len();
    store.begin()?;
    for ((x, y), tile) in tiles {
        if tile.is_empty() {
            continue;
        }
        let blob = format::encode(&tile);
        bytes += blob.len() as u64;
        store.put(x, y, region, &blob)?;
    }
    store.commit()?;

    Ok(Stats {
        nodes: n_pois,
        ways: n_ways,
        relations: n_rels,
        tiles: n_tiles,
        bytes,
    })
}
