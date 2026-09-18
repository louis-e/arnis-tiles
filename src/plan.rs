//! Picks the set of Geofabrik extracts to bake.
//!
//! Geofabrik's tree is not a partition: `us`, `dach`, `us-midwest` and friends overlap their
//! neighbours, and the index carries no file sizes, so "the smallest extract" cannot be read
//! off the tree. This solves it as weighted set cover over a lat/lon sample grid - cheapest
//! bytes per newly covered point first - which picks country and state extracts over the
//! continent-scale duplicates without any hand-maintained denylist.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const INDEX_URL: &str = "https://download.geofabrik.de/index-v1.json";
pub const USER_AGENT: &str = concat!(
    "arnis-tiles/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/louis-e/arnis)"
);

/// Extracts above this are never candidates: every one of them either has children that
/// cover it, or is a duplicate of regions that do (verified against the live index).
/// Baking a 35 GB continent to reach a few open-sea sample points is never worth it.
const MAX_EXTRACT_BYTES: u64 = 2_500_000_000;

/// Sample grid step in degrees. 0.25 deg is ~28 km at the equator - fine enough that no
/// mapped country is missed, coarse enough that the cover runs in seconds.
const GRID_STEP: f64 = 0.25;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub url: String,
    pub bytes: u64,
    pub continent: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub zoom: u8,
    pub regions: Vec<Region>,
    pub total_bytes: u64,
    /// Sample points no candidate covers. These are open sea beyond every cutting polygon;
    /// recorded so a real hole would show up as a number that is not small.
    pub uncovered_points: usize,
    pub covered_points: usize,
}

#[derive(Deserialize)]
struct Index {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    properties: Props,
    geometry: Option<Geometry>,
}

#[derive(Deserialize)]
struct Props {
    id: String,
    name: String,
    parent: Option<String>,
    urls: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Geometry {
    coordinates: serde_json::Value,
}

/// Outer rings only. Geofabrik's cutting polygons carry no holes, and a hole would only make
/// a candidate look smaller than it is - never create a gap.
fn rings(geom: &Option<Geometry>) -> Vec<Vec<(f64, f64)>> {
    let Some(g) = geom else { return Vec::new() };
    let mut out = Vec::new();
    // MultiPolygon: [ [ [ [lon,lat], ... ], hole... ], ... ]
    if let Some(polys) = g.coordinates.as_array() {
        for poly in polys {
            if let Some(ring_list) = poly.as_array() {
                if let Some(outer) = ring_list.first().and_then(|r| r.as_array()) {
                    let pts: Vec<(f64, f64)> = outer
                        .iter()
                        .filter_map(|p| {
                            let a = p.as_array()?;
                            Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                        })
                        .collect();
                    if pts.len() >= 3 {
                        out.push(pts);
                    }
                }
            }
        }
    }
    out
}

fn point_in_ring(lon: f64, lat: f64, ring: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > lat) != (yj > lat) && lon < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The first bytes of a .osm.pbf, which is enough for its OSMHeader.
///
/// Geofabrik publishes no cutting polygon for a handful of regions (Kazakhstan, Mongolia,
/// Israel/Palestine and friends - their .poly files are empty too, so this is deliberate on
/// their side, not a gap in the index). Without a shape they would drop out of the cover and
/// those countries would simply be missing from the archive. Every extract does carry a
/// bounding box in its header, and the header sits at the front of the file, so one range
/// request buys a usable shape. A box over-covers a cutting polygon, which can only make a
/// region look slightly bigger than it is - never leave a hole.
fn header_bbox(url: &str) -> Option<(f64, f64, f64, f64)> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .ok()?;
    let resp = client.get(url).header("Range", "bytes=0-65535").send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.bytes().ok()?;
    let reader = osmpbf::BlobReader::new(std::io::Cursor::new(body.to_vec()));
    for blob in reader {
        let Ok(blob) = blob else { return None };
        if let Ok(osmpbf::BlobDecode::OsmHeader(h)) = blob.decode() {
            let b = h.bbox()?;
            return Some((b.left, b.bottom, b.right, b.top));
        }
    }
    None
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{} for {url}", resp.status()));
    }
    Ok(resp.bytes().map_err(|e| e.to_string())?.to_vec())
}

fn content_length(client: &reqwest::blocking::Client, url: &str) -> Option<u64> {
    let resp = client.head(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

fn cached(
    cache: &Path,
    name: &str,
    fetch: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let p = cache.join(name);
    if let Ok(b) = std::fs::read(&p) {
        if !b.is_empty() {
            return Ok(b);
        }
    }
    let b = fetch()?;
    std::fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    std::fs::write(&p, &b).map_err(|e| e.to_string())?;
    Ok(b)
}

pub fn build(cache: &Path, zoom: u8) -> Result<Plan, String> {
    let raw = cached(cache, "index-v1.json", || {
        eprintln!("fetching {INDEX_URL}");
        http_get(INDEX_URL)
    })?;
    let index: Index = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;

    let with_pbf: Vec<(&Feature, String)> = index
        .features
        .iter()
        .filter_map(|f| f.properties.urls.get("pbf").map(|u| (f, u.clone())))
        .collect();

    // Sizes: one HEAD per extract, cached, because the index carries none.
    let sizes_raw = cached(cache, "sizes.json", || {
        eprintln!(
            "HEADing {} extracts for their sizes (once; cached)",
            with_pbf.len()
        );
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        let mut map: HashMap<String, u64> = HashMap::new();
        for (i, (f, url)) in with_pbf.iter().enumerate() {
            if let Some(n) = content_length(&client, url) {
                map.insert(f.properties.id.clone(), n);
            }
            if i % 50 == 0 {
                eprintln!("  {i}/{}", with_pbf.len());
            }
        }
        serde_json::to_vec_pretty(&map).map_err(|e| e.to_string())
    })?;
    let sizes: HashMap<String, u64> =
        serde_json::from_slice(&sizes_raw).map_err(|e| e.to_string())?;

    let parent: HashMap<String, Option<String>> = index
        .features
        .iter()
        .map(|f| (f.properties.id.clone(), f.properties.parent.clone()))
        .collect();
    let continent_of = |id: &str| -> String {
        let mut cur = id.to_string();
        for _ in 0..16 {
            match parent.get(&cur) {
                Some(Some(p)) => cur = p.clone(),
                _ => break,
            }
        }
        cur
    };

    // Universe: every grid point some extract covers.
    let mut points: Vec<(f64, f64)> = Vec::new();
    let mut lat = -85.0;
    while lat <= 85.0 {
        let mut lon = -180.0;
        while lon < 180.0 {
            points.push((lon, lat));
            lon += GRID_STEP;
        }
        lat += GRID_STEP;
    }
    eprintln!(
        "testing {} sample points against {} extracts",
        points.len(),
        with_pbf.len()
    );

    struct Cand<'a> {
        id: &'a str,
        name: &'a str,
        url: &'a str,
        bytes: u64,
        mask: Vec<u32>,
    }
    let mut cands: Vec<Cand> = Vec::new();
    let mut any_covered = vec![false; points.len()];

    let headers_raw = cached(cache, "header-bboxes.json", || {
        let missing: Vec<&(&Feature, String)> = with_pbf
            .iter()
            .filter(|(f, _)| rings(&f.geometry).is_empty())
            .collect();
        eprintln!("{} extracts have no cutting polygon; reading their .pbf headers", missing.len());
        let mut map: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
        for (f, url) in missing {
            match header_bbox(url) {
                Some(b) => {
                    eprintln!("  {} -> {:?}", f.properties.id, b);
                    map.insert(f.properties.id.clone(), b);
                }
                None => eprintln!("  {} -> NO HEADER BBOX (will be left to the Overpass fallback)", f.properties.id),
            }
        }
        serde_json::to_vec_pretty(&map).map_err(|e| e.to_string())
    })?;
    let headers: HashMap<String, (f64, f64, f64, f64)> =
        serde_json::from_slice(&headers_raw).map_err(|e| e.to_string())?;

    for (f, url) in &with_pbf {
        let mut rs = rings(&f.geometry);
        if rs.is_empty() {
            let Some((w, s, e, n)) = headers.get(&f.properties.id).copied() else {
                continue;
            };
            rs = vec![vec![(w, s), (e, s), (e, n), (w, n), (w, s)]];
        }
        let (mut w, mut s, mut e, mut n) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for r in &rs {
            for (x, y) in r {
                w = w.min(*x);
                e = e.max(*x);
                s = s.min(*y);
                n = n.max(*y);
            }
        }
        let mut mask = Vec::new();
        for (i, (lon, lat)) in points.iter().enumerate() {
            if *lon < w || *lon > e || *lat < s || *lat > n {
                continue;
            }
            if rs.iter().any(|r| point_in_ring(*lon, *lat, r)) {
                mask.push(i as u32);
                any_covered[i] = true;
            }
        }
        let bytes = sizes.get(&f.properties.id).copied().unwrap_or(u64::MAX);
        if mask.is_empty() || bytes > MAX_EXTRACT_BYTES {
            continue;
        }
        cands.push(Cand {
            id: &f.properties.id,
            name: &f.properties.name,
            url,
            bytes,
            mask,
        });
    }

    let universe: Vec<usize> = (0..points.len()).filter(|i| any_covered[*i]).collect();
    let mut needed: HashMap<u32, ()> = universe.iter().map(|i| (*i as u32, ())).collect();
    eprintln!(
        "universe: {} covered sample points, {} candidates",
        needed.len(),
        cands.len()
    );

    // Greedy weighted set cover: cheapest bytes per newly covered point wins each round.
    let mut chosen: Vec<Region> = Vec::new();
    let mut used = vec![false; cands.len()];
    loop {
        let mut best: Option<(usize, f64, usize)> = None;
        for (i, c) in cands.iter().enumerate() {
            if used[i] {
                continue;
            }
            let gain = c.mask.iter().filter(|p| needed.contains_key(p)).count();
            if gain == 0 {
                continue;
            }
            let cost = c.bytes as f64 / gain as f64;
            if best.is_none_or(|(_, b, _)| cost < b) {
                best = Some((i, cost, gain));
            }
        }
        let Some((i, _, _)) = best else { break };
        used[i] = true;
        for p in &cands[i].mask {
            needed.remove(p);
        }
        chosen.push(Region {
            id: cands[i].id.to_string(),
            name: cands[i].name.to_string(),
            url: cands[i].url.to_string(),
            bytes: cands[i].bytes,
            continent: continent_of(cands[i].id),
        });
    }

    // Greedy can leave a region that later picks made redundant - it commits to a choice
    // before seeing what comes after it. Dropping those is pure saved download.
    loop {
        let mut redundant = None;
        for i in 0..chosen.len() {
            let mine: HashSet<u32> = cands
                .iter()
                .find(|c| c.id == chosen[i].id)
                .map(|c| c.mask.iter().copied().collect())
                .unwrap_or_default();
            let others: HashSet<u32> = chosen
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .filter_map(|(_, r)| cands.iter().find(|c| c.id == r.id))
                .flat_map(|c| c.mask.iter().copied())
                .collect();
            if mine.is_subset(&others) {
                redundant = Some(i);
                break;
            }
        }
        match redundant {
            Some(i) => {
                eprintln!("  dropping {} - already covered by the rest", chosen[i].id);
                chosen.remove(i);
            }
            None => break,
        }
    }

    // Biggest first: the run then fails fast on a machine that cannot hold the largest
    // extract, instead of two days in.
    chosen.sort_by_key(|r| std::cmp::Reverse(r.bytes));
    let total_bytes = chosen.iter().map(|r| r.bytes).sum();

    Ok(Plan {
        zoom,
        total_bytes,
        uncovered_points: needed.len(),
        covered_points: universe.len(),
        regions: chosen,
    })
}
