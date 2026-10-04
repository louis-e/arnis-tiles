//! Where land is, from Natural Earth (public domain), rasterised once per plan.
//!
//! The plan only has to cover land. Sampling open sea made it pay 2.1 GB for alps to cover a
//! Mediterranean pocket only alps' polygon reaches, and left islands smaller than the sample
//! spacing with no samples at all.

use std::path::Path;

const NE_BASE: &str =
    "https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/10m_physical";

/// Raster cell in degrees, ~2.2 km. Cells a coastline passes through count as land, so the
/// mask errs towards land and an island smaller than a cell still shows up.
const RES: f64 = 0.02;
const W: usize = (360.0 / RES) as usize;
const H: usize = (180.0 / RES) as usize;

/// Islands smaller than this many degrees get samples of their own.
const SMALL_PART: f64 = 0.25;

type Ring = Vec<(f64, f64)>;

pub struct Land {
    bits: Vec<u64>,
}

impl Land {
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        let c = ((lon + 180.0) / RES).floor();
        let r = ((90.0 - lat) / RES).floor();
        if !(0.0..W as f64).contains(&c) || !(0.0..H as f64).contains(&r) {
            return false;
        }
        let i = r as usize * W + c as usize;
        self.bits[i / 64] >> (i % 64) & 1 == 1
    }

    fn set(&mut self, r: usize, c: usize, on: bool) {
        let i = r * W + c;
        if on {
            self.bits[i / 64] |= 1 << (i % 64);
        } else {
            self.bits[i / 64] &= !(1 << (i % 64));
        }
    }

    /// Even-odd fill of one record's rings at cell centres.
    fn fill(&mut self, rings: &[Ring], on: bool) {
        let mut hits: Vec<(usize, f64)> = Vec::new();
        for ring in rings {
            for k in 0..ring.len() {
                let (x1, y1) = ring[k];
                let (x2, y2) = ring[(k + 1) % ring.len()];
                if y1 == y2 {
                    continue;
                }
                let (lo, hi) = (y1.min(y2), y1.max(y2));
                let r_lo = (((90.0 - hi) / RES - 0.5).floor() + 1.0).max(0.0) as usize;
                let r_hi = ((90.0 - lo) / RES - 0.5).floor().min(H as f64 - 1.0);
                if r_hi < 0.0 {
                    continue;
                }
                for r in r_lo..=r_hi as usize {
                    let yc = 90.0 - (r as f64 + 0.5) * RES;
                    hits.push((r, x1 + (yc - y1) * (x2 - x1) / (y2 - y1)));
                }
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        for pair in hits.chunks_exact(2) {
            let ((r, xa), (r2, xb)) = (pair[0], pair[1]);
            if r != r2 {
                continue;
            }
            let c_lo = ((xa + 180.0) / RES - 0.5).ceil().max(0.0) as usize;
            let c_hi = ((xb + 180.0) / RES - 0.5).ceil() - 1.0;
            if c_hi < 0.0 {
                continue;
            }
            for c in c_lo..=(c_hi as usize).min(W - 1) {
                self.set(r, c, on);
            }
        }
    }

    /// Every cell a ring's edges pass through.
    fn mark_edges(&mut self, rings: &[Ring]) {
        for ring in rings {
            for k in 0..ring.len() {
                let (x1, y1) = ring[k];
                let (x2, y2) = ring[(k + 1) % ring.len()];
                let n = (((x2 - x1).hypot(y2 - y1)) / (RES / 2.0)).ceil().max(1.0) as usize;
                for t in 0..=n {
                    let f = t as f64 / n as f64;
                    let (x, y) = (x1 + (x2 - x1) * f, y1 + (y2 - y1) * f);
                    let c = ((x + 180.0) / RES).floor();
                    let r = ((90.0 - y) / RES).floor();
                    if (0.0..W as f64).contains(&c) && (0.0..H as f64).contains(&r) {
                        self.set(r as usize, c as usize, true);
                    }
                }
            }
        }
    }
}

/// Polygon records of an ESRI shapefile, each as its rings. Only x and y are read, so
/// PolygonZ and PolygonM work too.
fn read_shp(b: &[u8]) -> Result<Vec<Vec<Ring>>, String> {
    let i32_be = |o: usize| i32::from_be_bytes(b[o..o + 4].try_into().expect("4 bytes"));
    let i32_le = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().expect("4 bytes"));
    let f64_le = |o: usize| f64::from_le_bytes(b[o..o + 8].try_into().expect("8 bytes"));
    if b.len() < 100 || i32_be(0) != 9994 {
        return Err("not a shapefile".into());
    }
    let mut out = Vec::new();
    let mut pos = 100;
    while pos + 8 <= b.len() {
        let len = i32_be(pos + 4) as usize * 2;
        let rec = pos + 8;
        if rec + len > b.len() {
            return Err("truncated shapefile".into());
        }
        pos = rec + len;
        if len < 44 || ![5, 15, 25].contains(&i32_le(rec)) {
            continue;
        }
        let (np, npts) = (i32_le(rec + 36) as usize, i32_le(rec + 40) as usize);
        let pts_at = rec + 44 + 4 * np;
        if pts_at + 16 * npts > rec + len {
            return Err("bad polygon record".into());
        }
        let starts: Vec<usize> = (0..np).map(|k| i32_le(rec + 44 + 4 * k) as usize).collect();
        let mut rings = Vec::new();
        for k in 0..np {
            let end = starts.get(k + 1).copied().unwrap_or(npts).min(npts);
            let ring: Ring = (starts[k]..end)
                .map(|p| (f64_le(pts_at + 16 * p), f64_le(pts_at + 16 * p + 8)))
                .collect();
            if ring.len() >= 3 {
                rings.push(ring);
            }
        }
        out.push(rings);
    }
    Ok(out)
}

fn fetch(cache: &Path, name: &str) -> Result<Vec<Vec<Ring>>, String> {
    let raw = crate::plan::cached(cache, &format!("naturalearth/{name}"), || {
        eprintln!("fetching Natural Earth {name}");
        crate::plan::http_get(&format!("{NE_BASE}/{name}"))
    })?;
    read_shp(&raw).map_err(|e| format!("{name}: {e}"))
}

/// The land mask, and sample points inside every island too small for the sample grids.
pub fn load(cache: &Path) -> Result<(Land, Vec<(f64, f64)>), String> {
    // Natural Earth ships a tongue-in-cheek "Null Island" at 0,0.
    let real = |rec: &Vec<Ring>| {
        !rec.iter()
            .flatten()
            .all(|(x, y)| x.abs() < 0.1 && y.abs() < 0.1)
    };
    let land_recs: Vec<Vec<Ring>> = fetch(cache, "ne_10m_land.shp")?
        .into_iter()
        .filter(real)
        .collect();
    let islands: Vec<Vec<Ring>> = fetch(cache, "ne_10m_minor_islands.shp")?
        .into_iter()
        .filter(real)
        .collect();
    let lakes = fetch(cache, "ne_10m_lakes.shp")?;
    let mut land = Land {
        bits: vec![0; (W * H).div_ceil(64)],
    };
    for rec in land_recs.iter().chain(&islands) {
        land.fill(rec, true);
    }
    for rec in &lakes {
        land.fill(rec, false);
    }
    for rec in land_recs.iter().chain(&islands) {
        land.mark_edges(rec);
    }

    let mut extra = Vec::new();
    for ring in land_recs.iter().chain(&islands).flatten() {
        let (mut w, mut s, mut e, mut n) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (x, y) in ring {
            w = w.min(*x);
            e = e.max(*x);
            s = s.min(*y);
            n = n.max(*y);
        }
        if e - w >= SMALL_PART || n - s >= SMALL_PART {
            continue;
        }
        let before = extra.len();
        for i in 0..5 {
            for j in 0..5 {
                let x = w + (e - w) * (i as f64 + 0.5) / 5.0;
                let y = s + (n - s) * (j as f64 + 0.5) / 5.0;
                if crate::plan::point_in_ring(x, y, ring) {
                    extra.push((x, y));
                }
            }
        }
        if extra.len() == before {
            extra.push(ring[0]);
        }
    }
    Ok((land, extra))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Land {
        Land {
            bits: vec![0; (W * H).div_ceil(64)],
        }
    }

    #[test]
    fn fill_respects_holes() {
        let mut l = empty();
        let outer = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let hole = vec![(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0)];
        l.fill(&[outer, hole], true);
        assert!(l.contains(1.0, 1.0));
        assert!(!l.contains(5.0, 5.0));
        assert!(!l.contains(11.0, 5.0));
    }

    // A rock far smaller than a cell must still read as land.
    #[test]
    fn tiny_islands_survive_through_their_edges() {
        let mut l = empty();
        let rock = vec![(3.4100, -54.4270), (3.4105, -54.4270), (3.4105, -54.4265)];
        l.fill(std::slice::from_ref(&rock), true);
        l.mark_edges(&[rock]);
        assert!(l.contains(3.4102, -54.4268));
    }
}
