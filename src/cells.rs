//! Coarse coverage cells for the archive index.
//!
//! Continent bboxes are the source PBF's header bbox, which outlying territories stretch across
//! half the planet: north-america, russia and antarctica all span the full longitude range, so a
//! bbox test alone makes a client open six archives to read one. A cell list is small enough to
//! ship in the index and tells the client which archives can actually answer.

use std::collections::BTreeSet;
use std::path::Path;

/// Cell grid zoom. 64x64 worldwide, ~626 km per cell at the equator, so one archive's list is a
/// few hundred entries and the whole index stays a few kilobytes.
pub const CELL_ZOOM: u8 = 6;

pub fn cell_of(x: u32, y: u32, zoom: u8) -> u32 {
    let shift = zoom - CELL_ZOOM;
    let side = 1u32 << CELL_ZOOM;
    (y >> shift) * side + (x >> shift)
}

/// Tile coordinates for a PMTiles tile id (inverse Hilbert).
fn tile_xy(zoom: u8, tile_id: u64) -> (u32, u32) {
    let base = ((1u64 << (2 * zoom)) - 1) / 3;
    let mut t = tile_id.saturating_sub(base);
    let n = 1u64 << zoom;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x as u32, y as u32)
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

fn uvarint(b: &[u8], p: &mut usize) -> Result<u64, String> {
    let (mut out, mut shift) = (0u64, 0u32);
    loop {
        let c = *b.get(*p).ok_or("directory ended mid-varint")?;
        *p += 1;
        if shift >= 64 {
            return Err("varint overflow".into());
        }
        out |= u64::from(c & 0x7f) << shift;
        if c & 0x80 == 0 {
            return Ok(out);
        }
        shift += 7;
    }
}

/// (tile_id, run_length, length, offset) per entry.
fn decode_dir(buf: &[u8]) -> Result<Vec<(u64, u64, u64, u64)>, String> {
    let mut p = 0usize;
    let n = uvarint(buf, &mut p)? as usize;
    if n > 20_000_000 {
        return Err(format!("directory claims {n} entries"));
    }
    let mut ids = Vec::with_capacity(n);
    let mut last = 0u64;
    for _ in 0..n {
        last += uvarint(buf, &mut p)?;
        ids.push(last);
    }
    let mut runs = Vec::with_capacity(n);
    for _ in 0..n {
        runs.push(uvarint(buf, &mut p)?);
    }
    let mut lens = Vec::with_capacity(n);
    for _ in 0..n {
        lens.push(uvarint(buf, &mut p)?);
    }
    let mut offs: Vec<u64> = Vec::with_capacity(n);
    for i in 0..n {
        let v = uvarint(buf, &mut p)?;
        offs.push(if v == 0 && i > 0 {
            offs[i - 1] + lens[i - 1]
        } else {
            v - 1
        });
    }
    Ok((0..n)
        .map(|i| (ids[i], runs[i], lens[i], offs[i]))
        .collect())
}

fn inflate(kind: u8, raw: &[u8]) -> Result<Vec<u8>, String> {
    match kind {
        1 => Ok(raw.to_vec()),
        2 => {
            use std::io::Read;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(raw)
                .read_to_end(&mut out)
                .map_err(|e| format!("gzip directory: {e}"))?;
            Ok(out)
        }
        other => Err(format!("directory compression {other} not supported")),
    }
}

/// Every coverage cell an existing archive holds, read from its directories only.
pub fn scan_archive(path: &Path, zoom: u8) -> Result<Vec<u32>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut header = [0u8; 127];
    f.read_exact(&mut header)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if &header[..7] != b"PMTiles" || header[7] != 3 {
        return Err(format!("{} is not a PMTiles v3 archive", path.display()));
    }
    let (root_off, root_len) = (u64_at(&header, 8), u64_at(&header, 16));
    let leaf_off = u64_at(&header, 40);
    let internal = header[97];

    let mut read_at = |off: u64, len: u64| -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; len as usize];
        f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
        f.read_exact(&mut buf).map_err(|e| e.to_string())?;
        inflate(internal, &buf)
    };

    let mut cells = BTreeSet::new();
    let note = |id: u64, run: u64, cells: &mut BTreeSet<u32>| {
        for k in 0..run.min(1 << 20) {
            let (x, y) = tile_xy(zoom, id + k);
            cells.insert(cell_of(x, y, zoom));
        }
    };

    for (id, run, len, off) in decode_dir(&read_at(root_off, root_len)?)? {
        if run == 0 {
            for (lid, lrun, _, _) in decode_dir(&read_at(leaf_off + off, len)?)? {
                note(lid, lrun.max(1), &mut cells);
            }
        } else {
            note(id, run, &mut cells);
        }
    }
    Ok(cells.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmtiles::{TileCoord, TileId};

    // The inverse has to agree with the forward mapping the writer used, or a backfilled cell
    // list points at the wrong part of the world.
    #[test]
    fn tile_xy_inverts_the_writers_hilbert_order() {
        for (x, y) in [(0, 0), (1, 0), (4359, 2842), (8191, 8191), (5127, 4049)] {
            let id = u64::from(TileId::from(TileCoord::new(13, x, y).unwrap()));
            assert_eq!(tile_xy(13, id), (x, y), "tile 13/{x}/{y}");
        }
    }

    #[test]
    fn cells_group_neighbouring_tiles() {
        assert_eq!(cell_of(0, 0, 13), 0);
        assert_eq!(cell_of(127, 127, 13), 0);
        assert_eq!(cell_of(128, 0, 13), 1);
        assert_eq!(cell_of(0, 128, 13), 64);
        assert_eq!(cell_of(8191, 8191, 13), 4095);
    }
}
