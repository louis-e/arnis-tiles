//! Turns one .osm.pbf extract into per-tile payloads, written to the chunk store.
//!
//! Two streaming passes, so peak memory is one node-coordinate array rather than the whole
//! extract: node coordinates and relations first (the relations say which untagged ways are
//! multipolygon members), then ways. Nothing is held that a later pass can re-read. Each pass
//! decodes on every core; see `pass`.
//!
//! Finished tiles are flushed to the store as they accumulate rather than at the end. Holding a
//! whole region's output was what made the united-kingdom extract take the process past 8 GB;
//! a tile split across several flushes is merged at finalize like any other multi-writer tile.

use crate::format::{self, Node, Relation, Tile, Way};
use crate::store::{ChunkStore, RELATION_X};
use crate::tags;
use crate::tilemath;
use osmpbf::{Blob, BlobDecode, BlobReader, PrimitiveBlock, RelMemberType};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

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
    points * 8
        + tags
            .iter()
            .map(|(k, v)| k.len() + v.len() + 48)
            .sum::<usize>()
        + 32
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
    /// Tile ranges (x0, y0, x1, y1) to keep; empty keeps everything.
    keep: &'a [(u32, u32, u32, u32)],
    members: Vec<(u64, Vec<u8>)>,
    member_bytes: usize,
}

/// Flush threshold. Small enough that a dense extract stays well inside a laptop's memory,
/// large enough that the store sees big transactions rather than a row at a time.
const FLUSH_BYTES: usize = 192 * 1024 * 1024;

impl Sink<'_> {
    fn kept(&self, (x, y): (u32, u32)) -> bool {
        self.keep.is_empty()
            || self
                .keep
                .iter()
                .any(|(x0, y0, x1, y1)| (*x0..=*x1).contains(&x) && (*y0..=*y1).contains(&y))
    }

    /// Parks a relation member way on disk for the whole-relation records.
    fn member(&mut self, way: &Way) -> Result<(), String> {
        let blob = format::encode(&Tile {
            ways: vec![way.clone()],
            ..Tile::default()
        });
        self.member_bytes += blob.len();
        self.members.push((way.id, blob));
        if self.member_bytes >= FLUSH_BYTES / 4 {
            self.flush_members()?;
        }
        Ok(())
    }

    fn flush_members(&mut self) -> Result<(), String> {
        if self.members.is_empty() {
            return Ok(());
        }
        self.store.begin()?;
        for (id, blob) in self.members.drain(..) {
            self.store.put_member(id, self.region, &blob)?;
        }
        self.store.commit()?;
        self.member_bytes = 0;
        Ok(())
    }

    fn add(
        &mut self,
        tile: (u32, u32),
        weight: usize,
        f: impl FnOnce(&mut Tile),
    ) -> Result<(), String> {
        if !self.kept(tile) {
            return Ok(());
        }
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
    /// Relations also stored whole, because they span more than one tile.
    pub records: u64,
    pub tiles: usize,
    pub bytes: u64,
}

/// One pass over `pbf`: every core decodes blocks and runs `map` on them, and `consume` gets
/// the results one block at a time in file order, so the pass does exactly what a
/// single-threaded walk over the file would.
///
/// Decoding on one core held the bake to about 5 MB/s of .pbf on Modal, a tenth of what
/// Geofabrik serves there, while the other cores waited for the next region.
fn pass<T: Send>(
    pbf: &Path,
    map: impl Fn(&PrimitiveBlock) -> T + Sync,
    mut consume: impl FnMut(T) -> Result<(), String>,
) -> Result<(), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", pbf.display());
    let reader = BlobReader::from_path(pbf).map_err(|e| at(&e))?;
    // Modal reports the 20 cores a container may burst to. Past 16 the serial consumer is the
    // limit anyway, and each worker adds blocks in flight.
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get().min(16));
    in_order(
        reader.map(|blob| blob.map_err(|e| at(&e))),
        workers,
        |blob: Blob| match blob.decode() {
            Ok(BlobDecode::OsmData(block)) => Ok(Some(map(&block))),
            Ok(_) => Ok(None),
            Err(e) => Err(at(&e)),
        },
        |out| out.map_or(Ok(()), &mut consume),
    )
}

/// Runs `work` on each of `items` on `workers` threads, and hands the results to `consume` in
/// the order of `items`, as a plain loop over them would. Stops at the first error, whether it
/// comes from `items`, `work` (a panic included) or `consume`, and returns it once every thread
/// has finished.
fn in_order<I: Send, R: Send>(
    items: impl Iterator<Item = Result<I, String>> + Send,
    workers: usize,
    work: impl Fn(I) -> Result<R, String> + Sync,
    mut consume: impl FnMut(R) -> Result<(), String>,
) -> Result<(), String> {
    let workers = workers.max(1);
    // Items handed out but not yet consumed. A slow one holds up those behind it, and this
    // bounds how many can pile up in memory meanwhile: for the bake, a few MB each at most.
    let window = workers * 2;
    let (credit_tx, credit_rx) = mpsc::sync_channel::<()>(window);
    for _ in 0..window {
        credit_tx.send(()).expect("the receiver is right here");
    }
    let (item_tx, item_rx) = mpsc::sync_channel::<(usize, I)>(window);
    let item_rx = Arc::new(Mutex::new(item_rx));
    type Done<R> = (usize, Result<R, String>);
    let (done_tx, done_rx) = mpsc::channel::<Done<R>>();
    // Set on the first error, so the reader and the workers stop at their next item instead of
    // working through what is already in flight.
    let stop = AtomicBool::new(false);
    let (work, stop) = (&work, &stop);
    std::thread::scope(move |s| {
        let reader_done = done_tx.clone();
        s.spawn(move || {
            for (seq, item) in items.enumerate() {
                if credit_rx.recv().is_err() || stop.load(Ordering::Relaxed) {
                    return;
                }
                let sent = match item {
                    Ok(i) => item_tx.send((seq, i)).is_ok(),
                    Err(e) => {
                        let _ = reader_done.send((seq, Err(e)));
                        false
                    }
                };
                if !sent {
                    return;
                }
            }
        });
        for _ in 0..workers {
            let (rx, tx) = (Arc::clone(&item_rx), done_tx.clone());
            s.spawn(move || loop {
                let Ok((seq, item)) = rx.lock().expect("no worker panics").recv() else {
                    return;
                };
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                // A panic would leave the consumer waiting for this item forever, so it is
                // turned into an error like any other.
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(item)))
                    .unwrap_or_else(|_| Err(format!("worker panicked on item {seq}")));
                if tx.send((seq, out)).is_err() {
                    return;
                }
            });
        }
        drop(done_tx);
        drop(item_rx);

        let mut early: BTreeMap<usize, Result<R, String>> = BTreeMap::new();
        let mut next = 0;
        let mut run = || -> Result<(), String> {
            for (seq, out) in done_rx.iter() {
                early.insert(seq, out);
                while let Some(out) = early.remove(&next) {
                    next += 1;
                    consume(out?)?;
                    let _ = credit_tx.send(());
                }
            }
            // Only a worker that died without a word could leave a gap, but a gap must never
            // pass for the end.
            if early.is_empty() {
                Ok(())
            } else {
                Err(format!("item {next} never finished"))
            }
        };
        let result = run();
        if result.is_err() {
            stop.store(true, Ordering::Relaxed);
        }
        // Returning drops credit_tx and done_rx, which ends the reader and any worker still
        // waiting, so the scope can join them.
        result
    })
}

/// `keep` limits the output to those tile ranges, for supplements baked for a few islands.
pub fn bake(
    pbf: &Path,
    store: &mut ChunkStore,
    region: &str,
    zoom: u8,
    keep: &[(u32, u32, u32, u32)],
) -> Result<Stats, String> {
    // ── pass 1: node coordinates (+ the tagged ones, which are POIs in their own right), and
    //    relations, to learn which untagged ways are multipolygon members ──
    let mut relations: Vec<Relation> = Vec::new();
    let mut member_ways: HashSet<i64> = HashSet::new();
    let mut sink = Sink {
        tiles: HashMap::new(),
        weights: HashMap::new(),
        pending: 0,
        store,
        region,
        bytes: 0,
        rows: 0,
        keep,
        members: Vec::new(),
        member_bytes: 0,
    };
    let mut coords: Vec<(i64, i32, i32)> = Vec::new();
    let mut n_pois = 0u64;
    pass(
        pbf,
        |block| {
            let mut nodes: Vec<(i64, i32, i32)> = Vec::new();
            let mut pois: Vec<((u32, u32), Node)> = Vec::new();
            let mut rels: Vec<Relation> = Vec::new();
            let mut each = |id: i64, lat: f64, lon: f64, t: Vec<(String, String)>| {
                nodes.push((id, q(lat), q(lon)));
                if tags::node_is_wanted(&t) {
                    let node = Node {
                        id: id as u64,
                        lat: q(lat),
                        lon: q(lon),
                        tags: t,
                    };
                    pois.push((tilemath::tile_of(lat, lon, zoom), node));
                }
            };
            // Group by group, so the ways are skipped rather than walked. Their blobs are still
            // inflated: decode() unpacks a whole block.
            for group in block.groups() {
                for n in group.nodes() {
                    each(n.id(), n.lat(), n.lon(), tags::filter(n.tags()));
                }
                for n in group.dense_nodes() {
                    each(n.id(), n.lat(), n.lon(), tags::filter(n.tags()));
                }
                for r in group.relations() {
                    let t: Vec<(String, String)> = tags::filter(r.tags());
                    if !tags::relation_is_wanted(&t) {
                        continue;
                    }
                    let members: Vec<(u64, String)> = r
                        .members()
                        .filter(|m| m.member_type == RelMemberType::Way)
                        .map(|m| (m.member_id as u64, m.role().unwrap_or("").to_string()))
                        .collect();
                    if members.is_empty() {
                        continue;
                    }
                    rels.push(Relation {
                        id: r.id() as u64,
                        tags: t,
                        members,
                    });
                }
            }
            (nodes, pois, rels)
        },
        |(nodes, pois, rels)| {
            coords.extend(nodes);
            for (tl, node) in pois {
                n_pois += 1;
                let w = weigh(&node.tags, 1);
                sink.add(tl, w, |t| t.nodes.push(node))?;
            }
            for r in rels {
                for (id, _) in &r.members {
                    member_ways.insert(*id as i64);
                }
                relations.push(r);
            }
            Ok(())
        },
    )?;
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

    pass(
        pbf,
        |block| {
            let mut found = Vec::new();
            let mut each = |w: osmpbf::Way| {
                let t: Vec<(String, String)> = tags::filter(w.tags());
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
                    touched.push(tilemath::tile_of(deg(*lat), deg(*lon), zoom));
                }
                for pair in points.windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    tilemath::segment_tiles(
                        (deg(a.0), deg(a.1)),
                        (deg(b.0), deg(b.1)),
                        zoom,
                        &mut touched,
                    );
                }
                touched.sort_unstable();
                touched.dedup();
                let way = Way {
                    id: w.id() as u64,
                    closed,
                    tags: t,
                    points,
                };
                found.push((way, touched, is_member));
            };
            // Group by group, so the nodes and relations are skipped rather than walked, which
            // spares delta-decoding every dense node again.
            for group in block.groups() {
                for w in group.ways() {
                    each(w);
                }
            }
            found
        },
        |found| {
            for (way, touched, is_member) in found {
                let weight = weigh(&way.tags, way.points.len());
                for tl in &touched {
                    let copy = way.clone();
                    sink.add(*tl, weight, |t| t.ways.push(copy))?;
                }
                if is_member {
                    sink.member(&way)?;
                    way_tiles.insert(way.id, touched);
                }
                n_ways += 1;
            }
            Ok(())
        },
    )?;
    // The coordinate table is the biggest thing here and nothing below needs it.
    drop(coords);

    // A relation goes wherever its members went, so a tile that holds part of a multipolygon
    // also holds the relation that explains it.
    let mut n_rels = 0u64;
    sink.flush_members()?;
    let mut spans: Vec<(usize, Vec<(u32, u32)>)> = Vec::new();
    for (k, r) in relations.iter().enumerate() {
        let mut where_to: Vec<(u32, u32)> = r
            .members
            .iter()
            .filter_map(|(wid, _)| way_tiles.get(wid))
            .flatten()
            .copied()
            .collect();
        where_to.sort_unstable();
        where_to.dedup();
        if where_to.is_empty() {
            continue;
        }
        n_rels += 1;
        let weight = weigh(&r.tags, r.members.len());
        for t in &where_to {
            let copy = r.clone();
            sink.add(*t, weight, |tile| tile.relations.push(copy))?;
        }
        if where_to.len() > 1 && where_to.iter().any(|t| sink.kept(*t)) {
            spans.push((k, where_to));
        }
    }
    sink.flush()?;

    // A tile only holds the members that touch it, so a relation over several tiles is also
    // stored whole once; Arnis fetches it when a bbox has only part of it.
    let mut n_records = 0u64;
    sink.store.begin()?;
    for (k, _) in &spans {
        let r = &relations[*k];
        let Ok(rid) = u32::try_from(r.id) else {
            continue;
        };
        let mut record = Tile {
            relations: vec![r.clone()],
            ..Tile::default()
        };
        for (wid, _) in &r.members {
            if let Some(blob) = sink.store.member(*wid, region)? {
                record.ways.extend(format::decode(&blob)?.ways);
            }
        }
        record.ways.sort_by_key(|w| w.id);
        record.ways.dedup_by_key(|w| w.id);
        let blob = format::encode(&record);
        sink.store.put(RELATION_X, rid, region, &blob)?;
        sink.bytes += blob.len() as u64;
        n_records += 1;
    }
    sink.store.commit()?;
    sink.store.clear_members(region)?;

    Ok(Stats {
        nodes: n_pois,
        ways: n_ways,
        relations: n_rels,
        records: n_records,
        tiles: sink.rows,
        bytes: sink.bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::in_order;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Work that takes longer for some items than others, so they finish out of order.
    fn uneven(i: usize) -> Result<usize, String> {
        std::thread::sleep(std::time::Duration::from_micros(
            ((i * 7919) % 13) as u64 * 50,
        ));
        Ok(i * 3)
    }

    #[test]
    fn results_arrive_in_item_order() {
        for workers in [1, 2, 8, 16] {
            let mut seen = Vec::new();
            in_order((0..2000).map(Ok), workers, uneven, |r| {
                seen.push(r);
                Ok(())
            })
            .unwrap();
            assert_eq!(seen, (0..2000).map(|i| i * 3).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_bad_item_ends_the_pass_after_those_before_it() {
        let items = (0..500).map(|i| {
            if i == 300 {
                Err(format!("bad {i}"))
            } else {
                Ok(i)
            }
        });
        let mut seen = Vec::new();
        let r = in_order(items, 8, uneven, |r| {
            seen.push(r);
            Ok(())
        });
        assert_eq!(r, Err("bad 300".to_string()));
        assert_eq!(seen, (0..300).map(|i| i * 3).collect::<Vec<_>>());
    }

    #[test]
    fn failing_work_ends_the_pass_after_those_before_it() {
        let work = |i: usize| {
            if i == 77 {
                Err("no".to_string())
            } else {
                uneven(i)
            }
        };
        let mut seen = Vec::new();
        let r = in_order((0..500).map(Ok), 8, work, |r| {
            seen.push(r);
            Ok(())
        });
        assert_eq!(r, Err("no".to_string()));
        assert_eq!(seen, (0..77).map(|i| i * 3).collect::<Vec<_>>());
    }

    #[test]
    fn a_panic_is_an_error_not_a_hang() {
        let work = |i: usize| -> Result<usize, String> {
            if i == 5 {
                panic!("on purpose");
            }
            Ok(i)
        };
        let mut seen = Vec::new();
        let r = in_order((0..100).map(Ok), 4, work, |r| {
            seen.push(r);
            Ok(())
        });
        assert_eq!(r, Err("worker panicked on item 5".to_string()));
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_failing_consumer_stops_the_reading() {
        let read = AtomicUsize::new(0);
        let items = (0..100_000).map(|i| {
            read.fetch_add(1, Ordering::Relaxed);
            Ok(i)
        });
        let r = in_order(items, 8, uneven, |r| {
            if r == 30 {
                Err("full".to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(r, Err("full".to_string()));
        // Item 10 failed; at most a window of items (16) was read past it.
        assert!(read.load(Ordering::Relaxed) <= 10 + 1 + 16 + 1);
    }
}
