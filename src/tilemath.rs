//! Web-mercator slippy tile math. Same convention as Arnis's own tiler: y grows southward.

/// Archive zoom. Changing it invalidates every baked tile, so it is a build-wide constant.
pub const ZOOM: u8 = 13;

pub fn tile_of(lat: f64, lon: f64, z: u8) -> (u32, u32) {
    let n = (1u64 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0) as u32;
    // Mercator is undefined at the poles; clamp to the standard web-mercator limit.
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78);
    let r = lat.to_radians();
    let y = ((1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor()
        .clamp(0.0, n - 1.0) as u32;
    (x, y)
}

/// Fractional tile coordinates: the integer part is the tile, the rest is the position in it.
fn frac(lat: f64, lon: f64, z: u8) -> (f64, f64) {
    let n = (1u64 << z) as f64;
    let r = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    (
        (lon + 180.0) / 360.0 * n,
        (1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * n,
    )
}

/// Every tile the segment from `a` to `b` (lat, lon) passes through, appended to `out`.
///
/// A way stored only where its vertices fall is missing from a tile a long straight segment
/// crosses without a vertex in it, and a bbox inside that tile then has no road there.
pub fn segment_tiles(a: (f64, f64), b: (f64, f64), z: u8, out: &mut Vec<(u32, u32)>) {
    let max = (1i64 << z) - 1;
    let push = |x: i64, y: i64, out: &mut Vec<(u32, u32)>| {
        out.push((x.clamp(0, max) as u32, y.clamp(0, max) as u32));
    };
    let (x0, y0) = frac(a.0, a.1, z);
    let (x1, y1) = frac(b.0, b.1, z);
    let (mut cx, mut cy) = (x0.floor() as i64, y0.floor() as i64);
    let (ex, ey) = (x1.floor() as i64, y1.floor() as i64);
    push(cx, cy, out);
    // A jump across the antimeridian is not a segment that crosses the planet.
    if (ex - cx).abs() > max / 2 {
        push(ex, ey, out);
        return;
    }
    let (dx, dy) = (x1 - x0, y1 - y0);
    let (sx, sy) = (dx.signum() as i64, dy.signum() as i64);
    let next = |c: i64, d: f64, o: f64| {
        if d == 0.0 {
            f64::INFINITY
        } else {
            ((c + i64::from(d > 0.0)) as f64 - o) / d
        }
    };
    let (mut tx, mut ty) = (next(cx, dx, x0), next(cy, dy, y0));
    let (ddx, ddy) = (1.0 / dx.abs(), 1.0 / dy.abs());
    for _ in 0..(ex - cx).abs() + (ey - cy).abs() + 2 {
        if (cx, cy) == (ex, ey) {
            break;
        }
        if tx < ty {
            cx += sx;
            tx += ddx;
        } else if ty < tx {
            cy += sy;
            ty += ddy;
        } else {
            // Exactly through a corner: both neighbours touch the line.
            push(cx + sx, cy, out);
            push(cx, cy + sy, out);
            cx += sx;
            cy += sy;
            tx += ddx;
            ty += ddy;
        }
        push(cx, cy, out);
    }
}

/// (west, south, east, north) of a tile, in degrees.
pub fn tile_bounds(x: u32, y: u32, z: u8) -> (f64, f64, f64, f64) {
    let n = (1u64 << z) as f64;
    let west = x as f64 / n * 360.0 - 180.0;
    let east = (x + 1) as f64 / n * 360.0 - 180.0;
    let lat = |yy: f64| {
        (std::f64::consts::PI * (1.0 - 2.0 * yy / n))
            .sinh()
            .atan()
            .to_degrees()
    };
    (west, lat((y + 1) as f64), east, lat(y as f64))
}

/// Every tile overlapping a lat/lon box.
#[allow(dead_code)]
pub fn tiles_for_bbox(
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
    z: u8,
) -> Vec<(u32, u32)> {
    let (x1, y1) = tile_of(max_lat, min_lon, z);
    let (x2, y2) = tile_of(min_lat, max_lon, z);
    let (xlo, xhi) = (x1.min(x2), x1.max(x2));
    let (ylo, yhi) = (y1.min(y2), y1.max(y2));
    let mut out = Vec::new();
    for x in xlo..=xhi {
        for y in ylo..=yhi {
            out.push((x, y));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Slippy tiles are half-open, and the two axes close on opposite sides: the west and north
    // edges belong to the tile, the east and south edges to the neighbour. The equator and the
    // prime meridian are in this set on purpose - they are exactly where an off-by-one hides.
    #[test]
    fn a_tile_contains_the_point_it_was_derived_from() {
        for (lat, lon) in [
            (53.5511, 9.9937),
            (-33.8688, 151.2093),
            (0.0, 0.0),
            (64.14, -21.94),
            (-0.0001, -0.0001),
        ] {
            let (x, y) = tile_of(lat, lon, ZOOM);
            let (w, s, e, n) = tile_bounds(x, y, ZOOM);
            assert!(lon >= w && lon < e, "lon {lon} outside [{w},{e})");
            assert!(lat > s && lat <= n, "lat {lat} outside ({s},{n}]");
        }
    }

    #[test]
    fn bbox_covers_its_corners() {
        let tiles = tiles_for_bbox(53.545, 9.975, 53.565, 10.005, ZOOM);
        for (lat, lon) in [(53.545, 9.975), (53.565, 10.005), (53.55, 9.99)] {
            assert!(
                tiles.contains(&tile_of(lat, lon, ZOOM)),
                "{lat},{lon} missing"
            );
        }
    }

    #[test]
    fn segments_reach_tiles_without_vertices() {
        // Three tiles wide, vertices only in the outer two.
        let (w, s, e, n) = tile_bounds(100, 200, ZOOM);
        let (w2, _, e2, _) = tile_bounds(102, 200, ZOOM);
        let lat = (s + n) / 2.0;
        let mut out = Vec::new();
        segment_tiles((lat, (w + e) / 2.0), (lat, (w2 + e2) / 2.0), ZOOM, &mut out);
        out.sort_unstable();
        out.dedup();
        assert_eq!(out, vec![(100, 200), (101, 200), (102, 200)]);
    }

    #[test]
    fn antimeridian_jumps_stay_local() {
        let mut out = Vec::new();
        segment_tiles((-17.0, 179.99), (-17.0, -179.99), ZOOM, &mut out);
        assert!(out.len() <= 2, "{} tiles", out.len());
    }

    // The poles have no mercator row; they must clamp rather than produce a wild index.
    #[test]
    fn poles_clamp_into_range() {
        let max = (1u32 << ZOOM) - 1;
        for lat in [90.0, -90.0, 89.9] {
            let (_, y) = tile_of(lat, 0.0, ZOOM);
            assert!(y <= max);
        }
    }
}
