//! Merges the chunk store into one PMTiles archive per continent.
//!
//! A tile written by several extracts is concatenated and de-duplicated by OSM id here, which
//! is also where compression happens: one zstd frame per finished tile, so a client spends one
//! range request and one decompress per tile.

use crate::format::{self, Tile};
use crate::store::ChunkStore;
use pmtiles::{PmTilesWriter, TileCoord, TileType};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// One line of the archive directory a client reads before anything else.
#[derive(Serialize)]
struct ArchiveEntry {
    name: String,
    file: String,
    tiles: u64,
    bytes: u64,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
}

/// zstd level for published tiles. 19 costs bake time once and saves every download forever.
const ZSTD_LEVEL: i32 = 19;

fn merge(blobs: &[Vec<u8>]) -> Result<Tile, String> {
    let mut out = Tile::default();
    let (mut seen_n, mut seen_w, mut seen_r) = (HashSet::new(), HashSet::new(), HashSet::new());
    for b in blobs {
        let t = format::decode(b)?;
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

pub fn run(
    store: &ChunkStore,
    out_dir: &Path,
    zoom: u8,
    continent_of: &HashMap<String, String>,
) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    store.index_for_finalize()?;

    // Which continents are present, so each gets its own archive.
    let conn = store.connection();
    let mut regions_stmt = conn
        .prepare("SELECT DISTINCT region FROM chunk")
        .map_err(|e| e.to_string())?;
    let regions: Vec<String> = regions_stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let mut continents: Vec<String> = regions
        .iter()
        .map(|r| {
            continent_of
                .get(r)
                .cloned()
                .unwrap_or_else(|| "world".into())
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    continents.sort();

    let mut manifest: Vec<ArchiveEntry> = Vec::new();
    for continent in continents {
        let members: Vec<String> = regions
            .iter()
            .filter(|r| {
                continent_of
                    .get(*r)
                    .map(|c| c == &continent)
                    .unwrap_or(continent == "world")
            })
            .cloned()
            .collect();
        let placeholders = members.iter().map(|_| "?").collect::<Vec<_>>().join(",");

        // Tile ids first, so tiles can be fed to the writer in the order it wants without
        // holding the whole continent in memory.
        let sql = format!("SELECT DISTINCT x,y FROM chunk WHERE region IN ({placeholders})");
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let params = rusqlite::params_from_iter(members.iter());
        let mut coords: Vec<(u32, u32)> = stmt
            .query_map(params, |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        if coords.is_empty() {
            continue;
        }
        let extent = coords.iter().fold(
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
            |(w, s, e, n), (x, y)| {
                let (tw, ts, te, tn) = crate::tilemath::tile_bounds(*x, *y, zoom);
                (w.min(tw), s.min(ts), e.max(te), n.max(tn))
            },
        );
        let mut with_id: Vec<(u64, u32, u32)> = coords
            .drain(..)
            .filter_map(|(x, y)| {
                TileCoord::new(zoom, x, y)
                    .ok()
                    .map(|c| (u64::from(pmtiles::TileId::from(c)), x, y))
            })
            .collect();
        with_id.sort_unstable_by_key(|(id, _, _)| *id);

        let path = out_dir.join(format!("{continent}.pmtiles"));
        let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        let mut writer = PmTilesWriter::new(TileType::Unknown)
            .min_zoom(zoom)
            .max_zoom(zoom)
            .create(file)
            .map_err(|e| e.to_string())?;

        let mut blob_stmt = conn
            .prepare("SELECT data FROM chunk WHERE x=?1 AND y=?2")
            .map_err(|e| e.to_string())?;
        let (mut n, mut raw_total, mut comp_total) = (0u64, 0u64, 0u64);
        for (_, x, y) in &with_id {
            let blobs: Vec<Vec<u8>> = blob_stmt
                .query_map(rusqlite::params![x, y], |r| r.get::<_, Vec<u8>>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            let tile = merge(&blobs)?;
            if tile.is_empty() {
                continue;
            }
            let encoded = format::encode(&tile);
            let packed = zstd::encode_all(&encoded[..], ZSTD_LEVEL).map_err(|e| e.to_string())?;
            raw_total += encoded.len() as u64;
            comp_total += packed.len() as u64;
            let coord = TileCoord::new(zoom, *x, *y).map_err(|e| e.to_string())?;
            writer.add_tile(coord, &packed).map_err(|e| e.to_string())?;
            n += 1;
            if n % 50_000 == 0 {
                eprintln!(
                    "  {continent}: {n} tiles, {:.1} GB",
                    comp_total as f64 / 1e9
                );
            }
        }
        writer.finalize().map_err(|e| e.to_string())?;
        manifest.push(ArchiveEntry {
            name: continent.clone(),
            file: format!("{continent}.pmtiles"),
            tiles: n,
            bytes: comp_total,
            min_lat: extent.1,
            min_lon: extent.0,
            max_lat: extent.3,
            max_lon: extent.2,
        });
        eprintln!(
            "{}: {n} tiles, {:.2} GB raw -> {:.2} GB zstd ({})",
            path.display(),
            raw_total as f64 / 1e9,
            comp_total as f64 / 1e9,
            continent
        );
    }

    // The client reads this first and opens only the archives its bbox touches, so a bbox in
    // Hamburg never pays a request to find out that south-america does not have it.
    manifest.sort_by(|a, b| a.name.cmp(&b.name));
    let path = out_dir.join("archives.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "zoom": zoom,
            "format": "AOT1+zstd",
            "archives": manifest,
        }))
        .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    eprintln!("{} written", path.display());
    Ok(())
}
