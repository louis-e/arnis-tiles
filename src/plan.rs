//! Picks the set of extracts to bake.
//!
//! Geofabrik's tree is not a partition: `us`, `dach`, `us-midwest` and friends overlap their
//! neighbours, and the index carries no file sizes, so "the smallest extract" cannot be read
//! off the tree. This solves it as weighted set cover over sample points, cheapest bytes per
//! newly covered point first, which picks country and state extracts over the continent-scale
//! duplicates without a hand-maintained denylist.
//!
//! The samples are a global 0.25 deg grid plus a denser grid inside every polygon part, scaled
//! to that part. The global grid alone missed every extract it had no point of its own in:
//! Washington DC and Liechtenstein are smaller than a grid cell, every grid point near them was
//! also inside a neighbour, and neither was ever baked.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

const INDEX_URL: &str = "https://download.geofabrik.de/index-v1.json";
const OSMFR_EXTRACTS: &str = "https://download.openstreetmap.fr/extracts";
const OSMFR_POLYGONS: &str = "https://download.openstreetmap.fr/polygons";
pub const USER_AGENT: &str = concat!(
    "arnis-tiles/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/louis-e/arnis)"
);

/// Extracts above this are never candidates. Baking a 35 GB continent costs more RAM than the
/// bake has, and almost all of every continent is covered by its smaller children. What is
/// not (Saint Pierre and Miquelon, South Georgia, Diego Garcia and a few more) comes from the
/// supplements instead. 3 GB admits Geofabrik's japan, the only extract holding
/// Minami-Torishima and Okinotorishima; a 2.3 GB extract peaked at 4.7 GB of RAM.
const MAX_EXTRACT_BYTES: u64 = 3_000_000_000;

/// Global sample grid step in degrees, ~28 km at the equator.
const GRID_STEP: f64 = 0.25;

/// Each polygon part also gets about this many samples per side, so a part's sampling scales
/// with its own size rather than the planet's.
const PART_SAMPLES_PER_SIDE: f64 = 40.0;

/// Finest part sampling, ~200 m. Below this a part is a rock.
const MIN_PART_STEP: f64 = 0.002;

/// Margin around a supplement's clip cells, so land a sample point just missed stays inside.
const CLIP_MARGIN: f64 = 0.3;

/// Extracts from openstreetmap.fr for land no Geofabrik extract under the cap contains, and
/// anchors: places the plan must reach that are too small for any sample grid to hit.
const COVERAGE: &str = include_str!("../coverage.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub url: String,
    pub bytes: u64,
    pub continent: String,
    /// (west, south, east, north) boxes this region's tiles are kept within. Empty means the
    /// whole extract. Set on supplements, which are baked for a few islands and would
    /// otherwise duplicate a whole region the continent archives already hold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clip: Vec<[f64; 4]>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub zoom: u8,
    pub regions: Vec<Region>,
    pub total_bytes: u64,
    /// Land sample points no candidate covers.
    pub uncovered_points: usize,
    pub covered_points: usize,
    /// Land no candidate covers, as (lon, lat, samples) per 1 degree cell. Empty when the plan
    /// reaches every piece of land Natural Earth knows about.
    #[serde(default)]
    pub uncovered_land: Vec<[f64; 3]>,
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
    #[serde(rename = "type", default)]
    kind: String,
    coordinates: serde_json::Value,
}

#[derive(Deserialize)]
struct Coverage {
    supplements: Vec<Supplement>,
    anchors: Vec<Anchor>,
}

/// A supplement only ever covers land no Geofabrik extract covers, so it can never stand in
/// for one.
#[derive(Deserialize)]
struct Supplement {
    id: String,
    name: String,
    path: String,
    continent: String,
}

#[derive(Deserialize)]
struct Anchor {
    #[allow(dead_code)]
    name: String,
    /// (lon, lat)
    at: [f64; 2],
}

type Ring = Vec<(f64, f64)>;

/// A ring and its bounding box.
struct Edge {
    pts: Ring,
    bbox: (f64, f64, f64, f64),
}

impl Edge {
    fn new(pts: Ring) -> Self {
        let (mut w, mut s, mut e, mut n) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in &pts {
            w = w.min(*x);
            e = e.max(*x);
            s = s.min(*y);
            n = n.max(*y);
        }
        Edge {
            pts,
            bbox: (w, s, e, n),
        }
    }

    fn contains(&self, lon: f64, lat: f64) -> bool {
        let (w, s, e, n) = self.bbox;
        lon >= w && lon <= e && lat >= s && lat <= n && point_in_ring(lon, lat, &self.pts)
    }
}

pub(crate) fn point_in_ring(lon: f64, lat: f64, ring: &[(f64, f64)]) -> bool {
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

fn ring_of(v: &serde_json::Value) -> Ring {
    v.as_array()
        .map(|pts| {
            pts.iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every ring, outer or inner: membership is by parity, see [`Shape::contains`].
fn geojson_rings(geom: &Option<Geometry>) -> Vec<Edge> {
    let Some(g) = geom else { return Vec::new() };
    let polys: Vec<&serde_json::Value> = match g.kind.as_str() {
        "Polygon" => vec![&g.coordinates],
        _ => g
            .coordinates
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default(),
    };
    polys
        .into_iter()
        .filter_map(|poly| poly.as_array())
        .flatten()
        .map(ring_of)
        .filter(|r| r.len() >= 3)
        .map(Edge::new)
        .collect()
}

/// Osmosis .poly: a name line, then sections of "lon lat" lines closed by END. A '!' section
/// is a hole, which parity handles without being told.
fn poly_rings(text: &str) -> Vec<Edge> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim).skip(1);
    while let Some(head) = lines.next() {
        if head.is_empty() || head == "END" {
            continue;
        }
        let mut ring = Ring::new();
        for l in lines.by_ref() {
            if l == "END" {
                break;
            }
            let mut it = l.split_whitespace().map(|v| v.parse::<f64>());
            if let (Some(Ok(x)), Some(Ok(y))) = (it.next(), it.next()) {
                ring.push((x, y));
            }
        }
        if ring.len() >= 3 {
            out.push(Edge::new(ring));
        }
    }
    out
}

fn box_ring(w: f64, s: f64, e: f64, n: f64) -> Edge {
    Edge::new(vec![(w, s), (e, s), (e, n), (w, n), (w, s)])
}

/// The first bytes of a .osm.pbf, which is enough for its OSMHeader.
///
/// Geofabrik publishes no cutting polygon for a handful of regions (Kazakhstan, Mongolia,
/// Israel/Palestine and friends; their .poly files are empty too, so this is deliberate on
/// their side). Every extract carries a bounding box in its header, and the header sits at the
/// front of the file, so one range request buys a shape. It is only a box, though, so the plan
/// never lets it vouch for land a real polygon could cover.
fn header_bbox(url: &str) -> Option<(f64, f64, f64, f64)> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .ok()?;
    let resp = client
        .get(url)
        .header("Range", "bytes=0-65535")
        .send()
        .ok()?;
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

pub(crate) fn http_get(url: &str) -> Result<Vec<u8>, String> {
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

pub(crate) fn cached(
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
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, &b).map_err(|e| e.to_string())?;
    Ok(b)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Geofabrik extract with a cutting polygon.
    Polygon,
    /// Geofabrik extract known only by its header box. Always baked; covers only land no
    /// polygon covers.
    HeaderBox,
    /// openstreetmap.fr extract; covers only land no Geofabrik extract covers.
    Supplement,
    /// An openstreetmap.fr parent polygon, never baked. Their extracts are cut from the parent
    /// extract, so a child only holds what its parents hold too: their kanto polygon includes
    /// Minami-Torishima, their japan polygon does not, and neither does the kanto extract.
    Ancestor,
}

struct Shape {
    id: String,
    name: String,
    url: String,
    bytes: u64,
    continent: String,
    kind: Kind,
    rings: Vec<Edge>,
    parent: Option<String>,
}

impl Shape {
    /// Inside an odd number of rings, which is how the extracts are actually cut. That makes a
    /// hole a hole (South Africa's has Lesotho in it, and reading outer rings only credited all
    /// of Lesotho to South Africa) and also a ring nested in another one: Minami-Torishima sits
    /// inside two of Geofabrik's kanto rings, and the extract does not contain it.
    fn contains(&self, lon: f64, lat: f64) -> bool {
        self.rings.iter().filter(|r| r.contains(lon, lat)).count() % 2 == 1
    }

    fn bbox(&self) -> (f64, f64, f64, f64) {
        self.rings.iter().fold(
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
            |(w, s, e, n), p| {
                (
                    w.min(p.bbox.0),
                    s.min(p.bbox.1),
                    e.max(p.bbox.2),
                    n.max(p.bbox.3),
                )
            },
        )
    }
}

/// Global grid plus a grid inside every ring of every candidate scaled to that ring, kept
/// where there is land, plus `extra` (small islands and anchors) as given.
fn sample_points(
    shapes: &[Shape],
    land: Option<&crate::land::Land>,
    islands: &[(f64, f64)],
) -> Vec<(f64, f64)> {
    let on_land = |x: f64, y: f64| land.is_none_or(|l| l.contains(x, y));
    let mut points = Vec::new();
    let mut lat = -85.0;
    while lat <= 85.0 {
        let mut lon = -180.0;
        while lon < 180.0 {
            if on_land(lon, lat) {
                points.push((lon, lat));
            }
            lon += GRID_STEP;
        }
        lat += GRID_STEP;
    }
    points.extend_from_slice(islands);
    for shape in shapes.iter().filter(|s| s.bytes <= MAX_EXTRACT_BYTES) {
        for ring in &shape.rings {
            let (w, s, e, n) = ring.bbox;
            let step = (((e - w) * (n - s)).max(1e-12).sqrt() / PART_SAMPLES_PER_SIDE)
                .clamp(MIN_PART_STEP, GRID_STEP);
            let mut y = s + step / 2.0;
            while y < n {
                let mut x = w + step / 2.0;
                while x < e {
                    if ring.contains(x, y) && on_land(x, y) {
                        points.push((x, y));
                    }
                    x += step;
                }
                y += step;
            }
        }
    }
    points
}

/// Indices of the points inside each shape, via a 1 degree bucket index.
fn memberships(shapes: &[Shape], points: &[(f64, f64)]) -> Vec<Vec<u32>> {
    let mut buckets: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
    for (i, (lon, lat)) in points.iter().enumerate() {
        buckets
            .entry((lon.floor() as i32, lat.floor() as i32))
            .or_default()
            .push(i as u32);
    }
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out: Vec<std::sync::Mutex<Vec<u32>>> =
        shapes.iter().map(|_| std::sync::Mutex::default()).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(shape) = shapes.get(k) else { break };
                let (w, s, e, n) = shape.bbox();
                let mut inside = Vec::new();
                for bx in (w.floor() as i32)..=(e.floor() as i32) {
                    for by in (s.floor() as i32)..=(n.floor() as i32) {
                        let Some(idx) = buckets.get(&(bx, by)) else {
                            continue;
                        };
                        for &i in idx {
                            let (lon, lat) = points[i as usize];
                            if shape.contains(lon, lat) {
                                inside.push(i);
                            }
                        }
                    }
                }
                inside.sort_unstable();
                *out[k].lock().expect("no worker panics") = inside;
            });
        }
    });
    out.into_iter()
        .map(|m| m.into_inner().expect("no worker panics"))
        .collect()
}

/// 1 degree cells holding `pts`, widened by the margin, as clip boxes.
fn clip_boxes(pts: impl Iterator<Item = (f64, f64)>) -> Vec<[f64; 4]> {
    let cells: HashSet<(i32, i32)> = pts
        .map(|(lon, lat)| (lon.floor() as i32, lat.floor() as i32))
        .collect();
    let mut boxes: Vec<[f64; 4]> = cells
        .into_iter()
        .map(|(x, y)| {
            [
                (x as f64 - CLIP_MARGIN).max(-180.0),
                (y as f64 - CLIP_MARGIN).max(-90.0),
                (x as f64 + 1.0 + CLIP_MARGIN).min(180.0),
                (y as f64 + 1.0 + CLIP_MARGIN).min(90.0),
            ]
        })
        .collect();
    boxes.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    boxes
}

/// Every extract's shape, and the coverage anchors.
fn load_shapes(cache: &Path) -> Result<(Vec<Shape>, Ring), String> {
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

    let headers_raw = cached(cache, "header-bboxes.json", || {
        let missing: Vec<&(&Feature, String)> = with_pbf
            .iter()
            .filter(|(f, _)| geojson_rings(&f.geometry).is_empty())
            .collect();
        eprintln!(
            "{} extracts have no cutting polygon; reading their .pbf headers",
            missing.len()
        );
        let mut map: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
        for (f, url) in missing {
            match header_bbox(url) {
                Some(b) => {
                    eprintln!("  {} -> {:?}", f.properties.id, b);
                    map.insert(f.properties.id.clone(), b);
                }
                None => eprintln!(
                    "  {} -> NO HEADER BBOX (will be left to the Overpass fallback)",
                    f.properties.id
                ),
            }
        }
        serde_json::to_vec_pretty(&map).map_err(|e| e.to_string())
    })?;
    let headers: HashMap<String, (f64, f64, f64, f64)> =
        serde_json::from_slice(&headers_raw).map_err(|e| e.to_string())?;

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

    let mut shapes = Vec::new();
    for (f, url) in &with_pbf {
        let id = &f.properties.id;
        let mut rings = geojson_rings(&f.geometry);
        let mut kind = Kind::Polygon;
        if rings.is_empty() {
            let Some((w, s, e, n)) = headers.get(id).copied() else {
                continue;
            };
            rings = vec![box_ring(w, s, e, n)];
            kind = Kind::HeaderBox;
        }
        shapes.push(Shape {
            id: id.clone(),
            name: f.properties.name.clone(),
            url: url.clone(),
            bytes: sizes.get(id).copied().unwrap_or(u64::MAX),
            continent: continent_of(id),
            kind,
            rings,
            // Geofabrik children are not limited to their parent: French Guiana sits under
            // europe/france and its data is there all the same.
            parent: None,
        });
    }

    let Coverage {
        supplements,
        anchors,
    } = serde_json::from_str(COVERAGE).map_err(|e| format!("coverage.json: {e}"))?;
    let supp_sizes_raw = cached(cache, "supplement-sizes.json", || {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        let mut map: HashMap<String, u64> = HashMap::new();
        for s in &supplements {
            let url = format!("{OSMFR_EXTRACTS}/{}-latest.osm.pbf", s.path);
            if let Some(n) = content_length(&client, &url) {
                map.insert(s.id.clone(), n);
            }
        }
        serde_json::to_vec_pretty(&map).map_err(|e| e.to_string())
    })?;
    let supp_sizes: HashMap<String, u64> =
        serde_json::from_slice(&supp_sizes_raw).map_err(|e| e.to_string())?;
    let osmfr_rings = |path: &str| -> Result<Vec<Edge>, String> {
        let poly = cached(
            cache,
            &format!("supplements/{}.poly", path.replace('/', "_")),
            || http_get(&format!("{OSMFR_POLYGONS}/{path}.poly")),
        )?;
        Ok(poly_rings(&String::from_utf8_lossy(&poly)))
    };
    let osmfr_parent = |path: &str| path.rsplit_once('/').map(|(p, _)| format!("osmfr:{p}"));
    let mut ancestors: HashSet<String> = HashSet::new();
    for s in &supplements {
        let mut path = s.path.as_str();
        while let Some((up, _)) = path.rsplit_once('/') {
            if ancestors.insert(up.to_string()) {
                let rings = osmfr_rings(up)?;
                if rings.is_empty() {
                    return Err(format!("osmfr polygon {up} has no usable ring"));
                }
                shapes.push(Shape {
                    id: format!("osmfr:{up}"),
                    name: up.to_string(),
                    url: String::new(),
                    bytes: u64::MAX,
                    continent: String::new(),
                    kind: Kind::Ancestor,
                    rings,
                    parent: osmfr_parent(up),
                });
            }
            path = up;
        }
    }
    for s in supplements {
        let rings = osmfr_rings(&s.path)?;
        if rings.is_empty() {
            return Err(format!("{}: polygon has no usable ring", s.id));
        }
        let bytes = supp_sizes
            .get(&s.id)
            .copied()
            .ok_or_else(|| format!("{}: no size (is {} still published?)", s.id, s.path))?;
        shapes.push(Shape {
            url: format!("{OSMFR_EXTRACTS}/{}-latest.osm.pbf", s.path),
            id: s.id,
            name: s.name,
            bytes,
            continent: s.continent,
            kind: Kind::Supplement,
            rings,
            parent: osmfr_parent(&s.path),
        });
    }
    Ok((shapes, anchors.iter().map(|a| (a.at[0], a.at[1])).collect()))
}

/// `keep` names regions that are already published and stay. The plan then only adds what
/// they miss, which is how a gap is found and filled without re-baking anything.
pub fn build(cache: &Path, zoom: u8, keep: &HashSet<String>) -> Result<Plan, String> {
    let (shapes, anchors) = load_shapes(cache)?;
    let (land, mut extra) = crate::land::load(cache)?;
    extra.extend(anchors);
    let points = sample_points(&shapes, Some(&land), &extra);
    eprintln!(
        "testing {} sample points against {} extracts",
        points.len(),
        shapes.len()
    );
    let raw = memberships(&shapes, &points);
    // An openstreetmap.fr extract is cut from its parent, so it only holds points every
    // ancestor holds.
    let by_id: HashMap<&str, usize> = shapes
        .iter()
        .enumerate()
        .map(|(k, s)| (s.id.as_str(), k))
        .collect();
    let mut inside: Vec<Option<Vec<u32>>> = vec![None; shapes.len()];
    fn effective(
        k: usize,
        shapes: &[Shape],
        by_id: &HashMap<&str, usize>,
        raw: &[Vec<u32>],
        memo: &mut [Option<Vec<u32>>],
        depth: usize,
    ) -> Vec<u32> {
        if let Some(v) = &memo[k] {
            return v.clone();
        }
        let mut pts = raw[k].clone();
        if let Some(&up) = shapes[k].parent.as_deref().and_then(|p| by_id.get(p)) {
            if depth < 16 {
                let theirs: HashSet<u32> = effective(up, shapes, by_id, raw, memo, depth + 1)
                    .into_iter()
                    .collect();
                pts.retain(|p| theirs.contains(p));
            }
        }
        memo[k] = Some(pts.clone());
        pts
    }
    for k in 0..shapes.len() {
        effective(k, &shapes, &by_id, &raw, &mut inside, 0);
    }
    let inside: Vec<Vec<u32>> = inside.into_iter().map(Option::unwrap_or_default).collect();

    let candidate = |s: &Shape| s.bytes <= MAX_EXTRACT_BYTES && s.kind != Kind::Ancestor;
    // Points no Geofabrik polygon under the cap reaches. Boxes and supplements may only claim
    // these, so neither can ever let a real extract be skipped.
    let mut orphan = vec![true; points.len()];
    for (s, pts) in shapes.iter().zip(&inside) {
        for &p in pts {
            if s.kind == Kind::Polygon && candidate(s) {
                orphan[p as usize] = false;
            }
        }
    }

    struct Cand {
        shape: usize,
        mask: Vec<u32>,
        forced: bool,
    }
    let mut cands: Vec<Cand> = Vec::new();
    for (k, (s, pts)) in shapes.iter().zip(&inside).enumerate() {
        if !candidate(s) {
            continue;
        }
        let mask: Vec<u32> = match s.kind {
            Kind::Polygon => pts.clone(),
            Kind::HeaderBox | Kind::Supplement | Kind::Ancestor => pts
                .iter()
                .copied()
                .filter(|p| orphan[*p as usize])
                .collect(),
        };
        // A box-only country has no polygon to compete with, so it is baked whatever its box
        // happens to sample.
        let forced = s.kind == Kind::HeaderBox || keep.contains(&s.id);
        if mask.is_empty() && !forced {
            continue;
        }
        cands.push(Cand {
            shape: k,
            mask,
            forced,
        });
    }

    let mut needed: HashSet<u32> = (0..points.len() as u32).collect();
    let covered_points = needed.len();
    eprintln!(
        "universe: {covered_points} land sample points, {} candidates",
        cands.len()
    );

    let mut used = vec![false; cands.len()];
    let mut chosen: Vec<usize> = Vec::new();
    for (i, c) in cands.iter().enumerate() {
        if c.forced {
            used[i] = true;
            chosen.push(i);
            for p in &c.mask {
                needed.remove(p);
            }
        }
    }

    // Greedy weighted set cover: cheapest bytes per newly covered point wins each round.
    loop {
        let mut best: Option<(usize, f64)> = None;
        for (i, c) in cands.iter().enumerate() {
            if used[i] {
                continue;
            }
            let gain = c.mask.iter().filter(|p| needed.contains(p)).count();
            if gain == 0 {
                continue;
            }
            let cost = shapes[c.shape].bytes as f64 / gain as f64;
            if best.is_none_or(|(_, b)| cost < b) {
                best = Some((i, cost));
            }
        }
        let Some((i, _)) = best else { break };
        used[i] = true;
        chosen.push(i);
        for p in &cands[i].mask {
            needed.remove(p);
        }
    }

    let mut count = vec![0u16; points.len()];
    for &i in &chosen {
        for &p in &cands[i].mask {
            count[p as usize] += 1;
        }
    }
    let bytes_of = |i: usize| shapes[cands[i].shape].bytes;

    // Greedy commits before seeing later picks, so some early choice can end up fully covered
    // by the rest. Dropping those is pure saved download. Biggest first, and one pass is
    // enough: counts only fall, so a region kept once can never become redundant later.
    let prune = |chosen: &mut Vec<usize>, count: &mut Vec<u16>| {
        let mut by_size = chosen.clone();
        by_size.sort_by_key(|&i| std::cmp::Reverse(bytes_of(i)));
        for i in by_size {
            if cands[i].forced || !cands[i].mask.iter().all(|&p| count[p as usize] >= 2) {
                continue;
            }
            for &p in &cands[i].mask {
                count[p as usize] -= 1;
            }
            chosen.retain(|&c| c != i);
            eprintln!(
                "  dropping {}: already covered by the rest",
                shapes[cands[i].shape].id
            );
        }
    };
    prune(&mut chosen, &mut count);

    // Greedy also picks a big region early for what it covers at the time, and then keeps it
    // for a handful of points only it still covers: alps (2.1 GB) for Liechtenstein, which a
    // 3.5 MB extract covers. Swap such a region for a cheaper cover of its unique points.
    let mut covers: Vec<Vec<u32>> = vec![Vec::new(); points.len()];
    for (i, c) in cands.iter().enumerate() {
        for &p in &c.mask {
            covers[p as usize].push(i as u32);
        }
    }
    let mut by_size = chosen.clone();
    by_size.sort_by_key(|&i| std::cmp::Reverse(bytes_of(i)));
    for r in by_size {
        if cands[r].forced || !chosen.contains(&r) {
            continue;
        }
        let mut left: HashSet<u32> = cands[r]
            .mask
            .iter()
            .copied()
            .filter(|&p| count[p as usize] == 1)
            .collect();
        let budget = bytes_of(r);
        let (mut picks, mut cost) = (Vec::new(), 0u64);
        while !left.is_empty() {
            let mut gain: HashMap<u32, usize> = HashMap::new();
            for &p in &left {
                for &i in &covers[p as usize] {
                    if i as usize != r
                        && !chosen.contains(&(i as usize))
                        && !picks.contains(&(i as usize))
                    {
                        *gain.entry(i).or_default() += 1;
                    }
                }
            }
            let Some((&i, _)) = gain.iter().min_by(|a, b| {
                let ca = bytes_of(*a.0 as usize) as f64 / *a.1 as f64;
                let cb = bytes_of(*b.0 as usize) as f64 / *b.1 as f64;
                ca.total_cmp(&cb).then(a.0.cmp(b.0))
            }) else {
                break;
            };
            cost += bytes_of(i as usize);
            if cost >= budget {
                break;
            }
            picks.push(i as usize);
            for p in &cands[i as usize].mask {
                left.remove(p);
            }
        }
        if !left.is_empty() || cost >= budget {
            continue;
        }
        for &p in &cands[r].mask {
            count[p as usize] -= 1;
        }
        chosen.retain(|&c| c != r);
        for &i in &picks {
            for &p in &cands[i].mask {
                count[p as usize] += 1;
            }
            chosen.push(i);
        }
        let names: Vec<&str> = picks
            .iter()
            .map(|&i| shapes[cands[i].shape].id.as_str())
            .collect();
        eprintln!(
            "  swapping {} for {}",
            shapes[cands[r].shape].id,
            names.join(", ")
        );
    }
    prune(&mut chosen, &mut count);

    let mut regions: Vec<Region> = chosen
        .iter()
        .map(|&i| {
            let s = &shapes[cands[i].shape];
            // A supplement, or anything added to a published set, contributes only where it is
            // the sole cover: patching two islands with japan must not duplicate all of Japan.
            let clip = if s.kind == Kind::Supplement || (!keep.is_empty() && !keep.contains(&s.id))
            {
                clip_boxes(
                    cands[i]
                        .mask
                        .iter()
                        .filter(|&&p| count[p as usize] == 1)
                        .map(|&p| points[p as usize]),
                )
            } else {
                Vec::new()
            };
            Region {
                id: s.id.clone(),
                name: s.name.clone(),
                url: s.url.clone(),
                bytes: s.bytes,
                continent: s.continent.clone(),
                clip,
            }
        })
        .collect();

    // Biggest first: the run then fails fast on a machine that cannot hold the largest
    // extract, instead of two days in.
    regions.sort_by_key(|r| std::cmp::Reverse(r.bytes));
    let total_bytes = regions.iter().map(|r| r.bytes).sum();

    let mut cells: HashMap<(i32, i32), (f64, f64, usize)> = HashMap::new();
    for &p in &needed {
        let (lon, lat) = points[p as usize];
        let c = cells
            .entry((lon.floor() as i32, lat.floor() as i32))
            .or_insert((lon, lat, 0));
        c.2 += 1;
    }
    let mut uncovered_land: Vec<[f64; 3]> = cells
        .into_values()
        .map(|(lon, lat, n)| [lon, lat, n as f64])
        .collect();
    uncovered_land.sort_by(|a, b| b[2].total_cmp(&a[2]));

    Ok(Plan {
        zoom,
        total_bytes,
        uncovered_points: needed.len(),
        covered_points,
        uncovered_land,
        regions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(w: f64, s: f64, e: f64, n: f64) -> Ring {
        vec![(w, s), (e, s), (e, n), (w, n), (w, s)]
    }

    fn shape(rings: Vec<Ring>) -> Shape {
        Shape {
            id: "t".into(),
            name: "t".into(),
            url: String::new(),
            bytes: 1,
            continent: String::new(),
            kind: Kind::Polygon,
            rings: rings.into_iter().map(Edge::new).collect(),
            parent: None,
        }
    }

    // South Africa's polygon has a hole where Lesotho is. Ignoring it is how Lesotho went
    // missing from the archive.
    #[test]
    fn holes_are_not_inside() {
        let za = shape(vec![
            square(16.0, -35.0, 33.0, -22.0),
            square(27.0, -30.7, 29.5, -28.5),
        ]);
        assert!(za.contains(25.0, -26.0));
        assert!(!za.contains(28.0, -29.5));
    }

    // Geofabrik's kanto has Minami-Torishima inside two rings, and the extract leaves it out.
    #[test]
    fn nested_rings_cancel_out() {
        let kanto = shape(vec![
            square(130.0, 20.0, 155.0, 37.0),
            square(153.9, 24.2, 154.0, 24.4),
        ]);
        assert!(kanto.contains(139.7, 35.7));
        assert!(!kanto.contains(153.95, 24.3));
    }

    #[test]
    fn poly_files_parse_with_holes() {
        let text = "x\n1\n 0 0\n 10 0\n 10 10\n 0 10\nEND\n!2\n 4 4\n 6 4\n 6 6\n 4 6\nEND\nEND\n";
        let s = shape(Vec::new());
        let s = Shape {
            rings: poly_rings(text),
            ..s
        };
        assert_eq!(s.rings.len(), 2);
        assert!(s.contains(1.0, 1.0));
        assert!(!s.contains(5.0, 5.0));
    }

    // A part smaller than one global grid cell must still get points of its own: DC is
    // ~0.17 deg across and had none.
    #[test]
    fn small_parts_get_their_own_samples() {
        let dc = shape(vec![square(-77.12, 38.79, -76.91, 38.995)]);
        let n = sample_points(std::slice::from_ref(&dc), None, &[])
            .into_iter()
            .filter(|(x, y)| dc.contains(*x, *y))
            .count();
        assert!(n > 500, "only {n} samples inside DC");
    }
}
