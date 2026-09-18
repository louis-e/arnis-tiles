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
