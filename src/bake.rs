//! Turns one .osm.pbf extract into per-tile payloads, written to the chunk store.
//!
//! Three streaming passes, so peak memory is one node-coordinate array rather than the whole
//! extract: relations first (to learn which untagged ways are multipolygon members), then
//! node coordinates, then ways. Nothing is held that a later pass can re-read.
//!
//! Finished tiles are flushed to the store as they accumulate rather than at the end. Holding a
//! whole region's output was what made the united-kingdom extract take the process past 8 GB;
//! a tile split across several flushes is merged at finalize like any other multi-writer tile.

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

/// Roughly what an element costs in RAM, used only to decide when to flush. A String carries
/// its own allocation and header, so tags dominate and their length is what is counted.
fn weigh(tags: &[(String, String)], points: usize) -> usize {
    points * 8 + tags.iter().map(|(k, v)| k.len() + v.len() + 48).sum::<usize>() + 32
}

/// Accumulates tiles and spills them to the store on a memory budget.
struct Sink<'a> {
    tiles: HashMap<(u32, u32), Tile>,
    weights: HashMap<(u32, u32), usize>,
    pending: usize,
    store: &'a mut ChunkStore,
    region: &'a str,
    bytes: u64,
    rows: usize,
}

/// Flush threshold. Small enough that a dense extract stays well inside a laptop's memory,
/// large enough that the store sees big transactions rather than a row at a time.
const FLUSH_BYTES: usize = 192 * 1024 * 1024;

impl Sink<'_> {
    fn add(
        &mut self,
        tile: (u32, u32),
        weight: usize,
        f: impl FnOnce(&mut Tile),
    ) -> Result<(), String> {
        f(self.tiles.entry(tile).or_default());
        *self.weights.entry(tile).or_default() += weight;
        self.pending += weight;
        if self.pending >= FLUSH_BYTES {
            self.spill()?;
        }
        Ok(())
    }

    /// Writes out the heaviest tiles until the budget is half free again, instead of draining
    /// everything.
    ///
    /// Draining wrote every resident tile on every flush, which turned the united-kingdom into
    /// 1.07M rows over 46k tiles: 23 slivers each, none of them big enough for zstd to find
    /// anything, and a store barely smaller than the data in it. Evicting by weight means a
    /// dense tile is split a few times and the long tail of quiet tiles is written exactly once.
    fn spill(&mut self) -> Result<(), String> {
        let target = FLUSH_BYTES / 2;
        let mut by_weight: Vec<((u32, u32), usize)> =
            self.weights.iter().map(|(k, v)| (*k, *v)).collect();
        by_weight.sort_unstable_by_key(|(_, w)| std::cmp::Reverse(*w));

        let mut victims: Vec<(u32, u32)> = Vec::new();
        let mut freed = 0usize;
        for (key, w) in by_weight {
            if self.pending - freed <= target {
                break;
            }
            freed += w;
            victims.push(key);
        }
        self.write(&victims)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        let all: Vec<(u32, u32)> = self.tiles.keys().copied().collect();
        self.write(&all)
    }

    fn write(&mut self, keys: &[(u32, u32)]) -> Result<(), String> {
        if keys.is_empty() {
            return Ok(());
        }
        self.store.begin()?;
        for key in keys {
            let Some(tile) = self.tiles.remove(key) else {
                continue;
            };
            self.pending -= self.weights.remove(key).unwrap_or(0);
            if tile.is_empty() {
                continue;
            }
            let blob = format::encode(&tile);
            self.bytes += blob.len() as u64;
            self.rows += 1;
            self.store.put(key.0, key.1, self.region, &blob)?;
        }
        self.store.commit()?;
        Ok(())
    }
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
    let mut sink = Sink {
        tiles: HashMap::new(),
        weights: HashMap::new(),
        pending: 0,
        store,
        region,
        bytes: 0,
        rows: 0,
    };
    let mut coords: Vec<(i64, i32, i32)> = Vec::new();
    let mut n_pois = 0u64;
    let mut poi_err: Option<String> = None;
    let mut handle_node = |id: i64, lat: f64, lon: f64, t: Vec<(String, String)>| {
        coords.push((id, q(lat), q(lon)));
        if poi_err.is_none() && tags::node_is_wanted(&t) {
            n_pois += 1;
            let node = Node {
                id: id as u64,
                lat: q(lat),
                lon: q(lon),
                tags: t,
            };
            let tl = tilemath::tile_of(lat, lon, zoom);
            let w = weigh(&node.tags, 1);
            // osmpbf's for_each hands back no Result, so an error is parked and raised once
            // the pass ends rather than silently dropping the rest of the nodes.
            if let Err(e) = sink.add(tl, w, |t| t.nodes.push(node)) {
                poi_err = Some(e);
            }
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
    if let Some(e) = poi_err {
        return Err(e);
    }
    sink.flush()?;
    coords.sort_unstable_by_key(|(id, _, _)| *id);
    let lookup = |id: i64| -> Option<(i32, i32)> {
        coords
            .binary_search_by_key(&id, |(i, _, _)| *i)
            .ok()
            .map(|i| (coords[i].1, coords[i].2))
    };

    // ── pass 2: ways, emitted straight into their tiles ──────────────────────
    let mut way_tiles: HashMap<u64, Vec<(u32, u32)>> = HashMap::new();
    let mut n_ways = 0u64;
    let mut way_err: Option<String> = None;

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
            if way_err.is_some() {
                return;
            }
            let way = Way {
                id: w.id() as u64,
                closed,
                tags: t,
                points,
            };
            let weight = weigh(&way.tags, way.points.len());
            for tl in &touched {
                let copy = way.clone();
                if let Err(e) = sink.add(*tl, weight, |t| t.ways.push(copy)) {
                    way_err = Some(e);
                    return;
                }
            }
            if is_member {
                way_tiles.insert(way.id, touched);
            }
            n_ways += 1;
        })
        .map_err(|e| e.to_string())?;

    if let Some(e) = way_err {
        return Err(e);
    }
    // The coordinate table is the biggest thing here and nothing below needs it.
    drop(coords);

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
        let weight = weigh(&r.tags, r.members.len());
        for t in where_to {
            let copy = r.clone();
            sink.add(t, weight, |tile| tile.relations.push(copy))?;
        }
    }

    sink.flush()?;

    Ok(Stats {
        nodes: n_pois,
        ways: n_ways,
        relations: n_rels,
        tiles: sink.rows,
        bytes: sink.bytes,
    })
}
