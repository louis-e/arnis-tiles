//! The Arnis OSM tile payload ("AOT1").
//!
//! One tile holds everything Arnis needs to render the ground it covers: tagged POI nodes,
//! ways with inline geometry, and the relations whose members touch the tile. Way vertices
//! carry no OSM ids - the decoder mints them from the coordinate, which is what keeps the
//! archive about a third the size of the equivalent .osm.pbf.
//!
//! Coordinates are stored in units of 1e-5 degrees (~1.1 m). Arnis places one block per
//! metre, so that is below what it can render.

use std::collections::HashMap;

pub const MAGIC: &[u8; 4] = b"AOT1";
/// Degrees per stored coordinate unit.
pub const COORD_SCALE: f64 = 1e5;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: u64,
    pub lat: i32,
    pub lon: i32,
    pub tags: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Way {
    pub id: u64,
    /// First and last vertex are the same point. Stored explicitly: quantisation can make a
    /// ring look closed when it is not, and Arnis decides "is this a building outline" on it.
    pub closed: bool,
    pub tags: Vec<(String, String)>,
    /// (lat, lon) in COORD_SCALE units.
    pub points: Vec<(i32, i32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub id: u64,
    pub tags: Vec<(String, String)>,
    /// (way id, role). Arnis's ProcessedMember only ever holds ways.
    pub members: Vec<(u64, String)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tile {
    pub nodes: Vec<Node>,
    pub ways: Vec<Way>,
    pub relations: Vec<Relation>,
}

impl Tile {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.ways.is_empty() && self.relations.is_empty()
    }
}

// ── varints ───────────────────────────────────────────────────────────────────
fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_svarint(out: &mut Vec<u8>, v: i64) {
    put_uvarint(out, ((v << 1) ^ (v >> 63)) as u64);
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn uvarint(&mut self) -> Result<u64, String> {
        let mut out = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos).ok_or("truncated varint")?;
            self.pos += 1;
            if shift >= 64 {
                return Err("varint overflow".into());
            }
            out |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
        }
    }

    fn svarint(&mut self) -> Result<i64, String> {
        let v = self.uvarint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.buf.get(self.pos).ok_or("truncated")?;
        self.pos += 1;
        Ok(b)
    }
}

// ── encode ────────────────────────────────────────────────────────────────────
#[derive(Default)]
struct Strings {
    index: HashMap<String, u32>,
    order: Vec<String>,
}

impl Strings {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(i) = self.index.get(s) {
            return *i;
        }
        let i = self.order.len() as u32;
        self.order.push(s.to_string());
        self.index.insert(s.to_string(), i);
        i
    }
}

pub fn encode(tile: &Tile) -> Vec<u8> {
    let mut st = Strings::default();
    let mut body: Vec<u8> = Vec::new();

    let put_tags = |body: &mut Vec<u8>, tags: &[(String, String)], st: &mut Strings| {
        put_uvarint(body, tags.len() as u64);
        for (k, v) in tags {
            let (ki, vi) = (st.intern(k), st.intern(v));
            put_uvarint(body, ki as u64);
            put_uvarint(body, vi as u64);
        }
    };

    put_uvarint(&mut body, tile.nodes.len() as u64);
    let (mut pid, mut plat, mut plon) = (0i64, 0i64, 0i64);
    for n in &tile.nodes {
        put_svarint(&mut body, n.id as i64 - pid);
        put_svarint(&mut body, n.lat as i64 - plat);
        put_svarint(&mut body, n.lon as i64 - plon);
        pid = n.id as i64;
        plat = n.lat as i64;
        plon = n.lon as i64;
        put_tags(&mut body, &n.tags, &mut st);
    }

    put_uvarint(&mut body, tile.ways.len() as u64);
    let mut pid = 0i64;
    for w in &tile.ways {
        put_svarint(&mut body, w.id as i64 - pid);
        pid = w.id as i64;
        body.push(w.closed as u8);
        put_tags(&mut body, &w.tags, &mut st);
        put_uvarint(&mut body, w.points.len() as u64);
        let (mut a, mut o) = (0i64, 0i64);
        for (lat, lon) in &w.points {
            put_svarint(&mut body, *lat as i64 - a);
            put_svarint(&mut body, *lon as i64 - o);
            a = *lat as i64;
            o = *lon as i64;
        }
    }

    put_uvarint(&mut body, tile.relations.len() as u64);
    let mut pid = 0i64;
    for r in &tile.relations {
        put_svarint(&mut body, r.id as i64 - pid);
        pid = r.id as i64;
        put_tags(&mut body, &r.tags, &mut st);
        put_uvarint(&mut body, r.members.len() as u64);
        let mut pm = 0i64;
        for (wid, role) in &r.members {
            put_svarint(&mut body, *wid as i64 - pm);
            pm = *wid as i64;
            let ri = st.intern(role);
            put_uvarint(&mut body, ri as u64);
        }
    }

    // The string table is written first but interned while encoding the body, so it is
    // assembled last and prepended here.
    let mut out = Vec::with_capacity(body.len() + 1024);
    out.extend_from_slice(MAGIC);
    put_uvarint(&mut out, st.order.len() as u64);
    for s in &st.order {
        put_uvarint(&mut out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }
    out.extend_from_slice(&body);
    out
}

// ── decode ────────────────────────────────────────────────────────────────────
pub fn decode(buf: &[u8]) -> Result<Tile, String> {
    if buf.len() < 4 || &buf[..4] != MAGIC {
        return Err("not an AOT1 tile".into());
    }
    let mut c = Cursor { buf, pos: 4 };

    let n_strings = c.uvarint()?;
    if n_strings > 1 << 24 {
        return Err("implausible string table".into());
    }
    let mut strings: Vec<String> = Vec::with_capacity(n_strings.min(4096) as usize);
    for _ in 0..n_strings {
        let len = c.uvarint()? as usize;
        let end = c.pos.checked_add(len).ok_or("string overflow")?;
        let s = buf.get(c.pos..end).ok_or("truncated string")?;
        strings.push(String::from_utf8(s.to_vec()).map_err(|e| e.to_string())?);
        c.pos = end;
    }
    let s_at = |i: u64| -> Result<String, String> {
        strings
            .get(i as usize)
            .cloned()
            .ok_or_else(|| "string index out of range".to_string())
    };

    let read_tags = |c: &mut Cursor| -> Result<Vec<(String, String)>, String> {
        let n = c.uvarint()?;
        if n > 1 << 16 {
            return Err("implausible tag count".into());
        }
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let k = c.uvarint()?;
            let v = c.uvarint()?;
            out.push((s_at(k)?, s_at(v)?));
        }
        Ok(out)
    };

    let mut tile = Tile::default();

    let n_nodes = c.uvarint()?;
    let (mut pid, mut plat, mut plon) = (0i64, 0i64, 0i64);
    for _ in 0..n_nodes {
        pid += c.svarint()?;
        plat += c.svarint()?;
        plon += c.svarint()?;
        let tags = read_tags(&mut c)?;
        tile.nodes.push(Node {
            id: pid as u64,
            lat: plat as i32,
            lon: plon as i32,
            tags,
        });
    }

    let n_ways = c.uvarint()?;
    let mut pid = 0i64;
    for _ in 0..n_ways {
        pid += c.svarint()?;
        let closed = c.byte()? != 0;
        let tags = read_tags(&mut c)?;
        let n_pts = c.uvarint()?;
        if n_pts > 1 << 22 {
            return Err("implausible vertex count".into());
        }
        let mut points = Vec::with_capacity(n_pts as usize);
        let (mut a, mut o) = (0i64, 0i64);
        for _ in 0..n_pts {
            a += c.svarint()?;
            o += c.svarint()?;
            points.push((a as i32, o as i32));
        }
        tile.ways.push(Way {
            id: pid as u64,
            closed,
            tags,
            points,
        });
    }

    let n_rels = c.uvarint()?;
    let mut pid = 0i64;
    for _ in 0..n_rels {
        pid += c.svarint()?;
        let tags = read_tags(&mut c)?;
        let n_mem = c.uvarint()?;
        if n_mem > 1 << 20 {
            return Err("implausible member count".into());
        }
        let mut members = Vec::with_capacity(n_mem as usize);
        let mut pm = 0i64;
        for _ in 0..n_mem {
            pm += c.svarint()?;
            let role = c.uvarint()?;
            members.push((pm as u64, s_at(role)?));
        }
        tile.relations.push(Relation {
            id: pid as u64,
            tags,
            members,
        });
    }

    Ok(tile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn round_trips_every_element_kind() {
        let tile = Tile {
            nodes: vec![Node {
                id: 4_000_000_001,
                lat: 5_355_110,
                lon: 999_370,
                tags: tags(&[("amenity", "waste_basket")]),
            }],
            ways: vec![
                Way {
                    id: 27,
                    closed: true,
                    tags: tags(&[("building", "yes"), ("building:levels", "4")]),
                    points: vec![(10, 10), (10, 20), (20, 20), (10, 10)],
                },
                Way {
                    id: 28,
                    closed: false,
                    tags: tags(&[("highway", "residential")]),
                    points: vec![(-5, -5), (0, 0)],
                },
            ],
            relations: vec![Relation {
                id: 9,
                tags: tags(&[("type", "multipolygon"), ("building", "yes")]),
                members: vec![(27, "outer".into()), (28, "inner".into())],
            }],
        };
        assert_eq!(decode(&encode(&tile)).unwrap(), tile);
    }

    #[test]
    fn an_empty_tile_round_trips() {
        let t = Tile::default();
        assert_eq!(decode(&encode(&t)).unwrap(), t);
    }

    // A tile that has been cut short must be reported, not silently decoded into a
    // half-world: a truncated tile and a sparse one are indistinguishable downstream.
    #[test]
    fn truncation_is_an_error_not_a_short_tile() {
        let tile = Tile {
            ways: vec![Way {
                id: 1,
                closed: false,
                tags: tags(&[("highway", "path")]),
                points: vec![(1, 1), (2, 2), (3, 3)],
            }],
            ..Default::default()
        };
        let full = encode(&tile);
        for cut in [5, full.len() / 2, full.len() - 1] {
            assert!(decode(&full[..cut]).is_err(), "cut at {cut} must fail");
        }
    }

    #[test]
    fn a_foreign_payload_is_rejected() {
        assert!(decode(b"NOPE1234").is_err());
        assert!(decode(b"").is_err());
    }
}
