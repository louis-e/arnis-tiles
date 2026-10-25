//! Merges one continent's chunk store into its PMTiles archive.
//!
//! A tile written by several extracts - or flushed in pieces by one - is concatenated and
//! de-duplicated by OSM id here, which is also where compression happens: one zstd frame per
//! finished tile, so a client spends one range request and one decompress per tile.

use crate::format::{self, Tile};
use crate::store::{ChunkStore, RELATION_X};
use pmtiles::{PmTilesWriter, TileCoord, TileType};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

/// zstd level for published tiles. 19 costs bake time once and saves every download forever.
const ZSTD_LEVEL: i32 = 19;

/// UTC (YYYYMMDD, YYYYMMDD-HHMMSS) of this build. The file name takes the time too: two bakes
/// of a continent on one day must not share a name, or a client could read ranges cached from
/// the first file against the second.
fn build_stamp() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let date = date_from_secs(secs);
    let t = secs.rem_euclid(86_400);
    let stamp = format!("{date}-{:02}{:02}{:02}", t / 3600, t / 60 % 60, t % 60);
    (date, stamp)
}

/// UTC date as YYYYMMDD, via Howard Hinnant's civil-from-days.
pub fn date_from_secs(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

/// One line of the archive directory a client reads before anything else.
#[derive(Serialize, Deserialize, Clone)]
pub struct ArchiveEntry {
    pub name: String,
    /// Dated, so every published archive is immutable: a re-bake lands beside the old file
    /// instead of overwriting it, which is what lets clients cache byte ranges safely.
    pub file: String,
    #[serde(default)]
    pub built: String,
    pub tiles: u64,
    pub bytes: u64,
    /// Coarse cells this archive actually holds, so a client can skip it without opening it.
    #[serde(default)]
    pub cells: Vec<u32>,
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

fn merge(blobs: &[Vec<u8>]) -> Result<Tile, String> {
    let mut out = Tile::default();
    let (mut seen_n, mut seen_w, mut seen_r) = (HashSet::new(), HashSet::new(), HashSet::new());
    for packed in blobs {
        let b = ChunkStore::unpack(packed)?;
        let t = format::decode(&b)?;
        for n in t.nodes {
            if seen_n.insert(n.id) {
                out.nodes.push(n);
            }
        }
        for w in t.ways {
            if seen_w.insert(w.id) {
                out.ways.push(w);
            }
        }
        for r in t.relations {
            if seen_r.insert(r.id) {
                out.relations.push(r);
            }
        }
    }
    // Stable output regardless of which extract was baked first, so two runs of the same
    // planet produce byte-identical tiles.
    out.nodes.sort_by_key(|n| n.id);
    out.ways.sort_by_key(|w| w.id);
    out.relations.sort_by_key(|r| r.id);
    Ok(out)
}

/// Tiles per round: enough that every worker stays busy, few enough that a round of dense city
/// tiles held for the writer stays in the hundreds of megabytes.
const ROUND: usize = 2048;

/// A merged tile's zstd frame and its size before compression; None for a tile left empty.
type Packed = Option<(Vec<u8>, u64)>;

/// A reused bulk compressor rather than `encode_all`, which sets up a full-size level-19 context
/// on every call: 0.8 ms before compressing anything, more than the median tile then takes, and
/// on ten threads at once four times the CPU in all. Knowing each tile's size also lets zstd fit
/// its tables to it. The frames decompress to the same bytes and come out about 0.2% larger.
fn pack(
    zstd: &mut zstd::bulk::Compressor,
    stmt: &mut rusqlite::CachedStatement,
    (x, y): (u32, u32),
    keep_empty: bool,
) -> Result<Packed, String> {
    let blobs: Vec<Vec<u8>> = stmt
        .query_map(rusqlite::params![x, y], |r| r.get::<_, Vec<u8>>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let merged = merge(&blobs)?;
    if merged.is_empty() && !keep_empty {
        return Ok(None);
    }
    let encoded = format::encode(&merged);
    let packed = zstd.compress(&encoded).map_err(|e| e.to_string())?;
    Ok(Some((packed, encoded.len() as u64)))
}

/// Reads, merges and compresses `keys` on one worker per reader connection, handing each result
/// to `emit` in the order of `keys`, which is the order the PMTiles writer needs.
///
/// Done one tile at a time this was most of a run: North America's archive took eight hours on
/// one core, 3.4 million small records each waiting on its own chunk reads and then its own
/// compression. Each worker has its own connection so the reads overlap as well, and takes the
/// next tile as it frees up rather than a fixed share, because Hilbert order clusters a city's
/// heavy tiles into the same stretch.
fn pack_all<K: Copy + Sync>(
    readers: &mut [Connection],
    keys: &[K],
    at: impl Fn(K) -> (u32, u32) + Sync,
    keep_empty: bool,
    mut emit: impl FnMut(K, Vec<u8>, u64) -> Result<(), String>,
) -> Result<(), String> {
    for round in keys.chunks(ROUND) {
        let next = AtomicUsize::new(0);
        let done: Vec<Vec<(usize, Packed)>> = std::thread::scope(|s| {
            let workers: Vec<_> = readers
                .iter_mut()
                .map(|conn| {
                    let (next, at) = (&next, &at);
                    s.spawn(move || -> Result<Vec<(usize, Packed)>, String> {
                        let mut stmt = conn
                            .prepare_cached("SELECT data FROM chunk WHERE x=?1 AND y=?2")
                            .map_err(|e| e.to_string())?;
                        let mut zstd =
                            zstd::bulk::Compressor::new(ZSTD_LEVEL).map_err(|e| e.to_string())?;
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&k) = round.get(i) else {
                                return Ok(mine);
                            };
                            mine.push((i, pack(&mut zstd, &mut stmt, at(k), keep_empty)?));
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().map_err(|_| "finalize worker panicked")?)
                .collect::<Result<_, String>>()
        })?;
        let mut packed: Vec<Packed> = (0..round.len()).map(|_| None).collect();
        for (i, p) in done.into_iter().flatten() {
            packed[i] = p;
        }
        for (&k, p) in round.iter().zip(packed) {
            if let Some((bytes, raw)) = p {
                emit(k, bytes, raw)?;
            }
        }
    }
    Ok(())
}

pub fn one(
    store: &ChunkStore,
    out_dir: &Path,
    zoom: u8,
    continent: &str,
) -> Result<Option<ArchiveEntry>, String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    store.index_for_finalize()?;
    let conn = store.connection();

    let mut stmt = conn
        .prepare(&format!(
            "SELECT DISTINCT x,y FROM chunk WHERE x<>{RELATION_X}"
        ))
        .map_err(|e| e.to_string())?;
    let coords: Vec<(u32, u32)> = stmt
        .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if coords.is_empty() {
        return Ok(None);
    }

    let extent = coords.iter().fold(
        (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
        |(w, s, e, n), (x, y)| {
            let (tw, ts, te, tn) = crate::tilemath::tile_bounds(*x, *y, zoom);
            (w.min(tw), s.min(ts), e.max(te), n.max(tn))
        },
    );

    // PMTiles wants tiles in Hilbert order, which is not (x, y) order.
    let mut with_id: Vec<(u64, u32, u32)> = coords
        .into_iter()
        .filter_map(|(x, y)| {
            TileCoord::new(zoom, x, y)
                .ok()
                .map(|c| (u64::from(pmtiles::TileId::from(c)), x, y))
        })
        .collect();
    with_id.sort_unstable_by_key(|(id, _, _)| *id);

    let (built, stamp) = build_stamp();
    let path = out_dir.join(format!("{continent}-{stamp}.pmtiles"));
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut writer = PmTilesWriter::new(TileType::Unknown)
        .min_zoom(zoom)
        .max_zoom(zoom)
        .create(file)
        .map_err(|e| e.to_string())?;

    // One read-only connection per worker. The store is in WAL mode, so they read side by side.
    let db = conn.path().ok_or("the chunk store has no file")?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut readers = (0..threads)
        .map(|_| Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    eprintln!("  {continent}: {threads} workers");
    let (mut n, mut raw_total, mut comp_total) = (0u64, 0u64, 0u64);
    let mut cells = std::collections::BTreeSet::new();
    pack_all(
        &mut readers,
        &with_id,
        |(_, x, y)| (x, y),
        false,
        |(_, x, y), packed, raw| {
            raw_total += raw;
            comp_total += packed.len() as u64;
            let coord = TileCoord::new(zoom, x, y).map_err(|e| e.to_string())?;
            writer.add_tile(coord, &packed).map_err(|e| e.to_string())?;
            cells.insert(crate::cells::cell_of(x, y, zoom));
            n += 1;
            if n % 50_000 == 0 {
                eprintln!(
                    "  {continent}: {n} tiles, {:.1} GB",
                    comp_total as f64 / 1e9
                );
            }
            Ok(())
        },
    )?;
    // Whole relations after the tiles: their ids sit above every zoom 13 id, so the writer still
    // sees ids in increasing order.
    let mut rel_stmt = conn
        .prepare(&format!(
            "SELECT DISTINCT y FROM chunk WHERE x={RELATION_X} ORDER BY y"
        ))
        .map_err(|e| e.to_string())?;
    let rel_ids: Vec<u32> = rel_stmt
        .query_map([], |r| r.get::<_, u32>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut records = 0u64;
    pack_all(
        &mut readers,
        &rel_ids,
        |rid| (RELATION_X, rid),
        true,
        |rid, packed, raw| {
            raw_total += raw;
            comp_total += packed.len() as u64;
            let id = pmtiles::TileId::new(format::RELATION_TILE_BASE + u64::from(rid))
                .map_err(|e| e.to_string())?;
            writer
                .add_tile(TileCoord::from(id), &packed)
                .map_err(|e| e.to_string())?;
            records += 1;
            Ok(())
        },
    )?;
    eprintln!("  {continent}: {records} whole relations");
    writer.finalize().map_err(|e| e.to_string())?;
    eprintln!(
        "{}: {n} tiles, {:.2} GB raw -> {:.2} GB zstd",
        path.display(),
        raw_total as f64 / 1e9,
        comp_total as f64 / 1e9,
    );

    Ok(Some(ArchiveEntry {
        name: continent.to_string(),
        file: format!("{continent}-{stamp}.pmtiles"),
        built,
        tiles: n,
        cells: cells.into_iter().collect(),
        // On disk, not the sum of the tiles: the writer stores identical tiles once.
        bytes: std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(comp_total),
        min_lat: extent.1,
        min_lon: extent.0,
        max_lat: extent.3,
        max_lon: extent.2,
    }))
}
