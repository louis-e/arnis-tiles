//! Which OSM elements and tags the archive carries.
//!
//! Mirrors two things in Arnis: the key list of the Overpass query in `retrieve_data.rs`, and
//! the tag filter in `osm_parser.rs`. Drifting from either shows up as detail quietly missing
//! from generated worlds, so both are reproduced here in full rather than approximated.

/// Keys that make an element worth carrying at all.
const WANTED_KEYS: &[&str] = &[
    "building",
    "building:part",
    "highway",
    "landuse",
    "natural",
    "leisure",
    "water",
    "waterway",
    "amenity",
    "tourism",
    "bridge",
    "railway",
    "roller_coaster",
    "barrier",
    "entrance",
    "door",
    "power",
    "historic",
    "emergency",
    "advertising",
    "man_made",
    "aeroway",
    "3dmr",
    "shop",
    "office",
];

const IGNORED_TAGS: &[&str] = &[
    "created_by",
    "note",
    "fixme",
    "FIXME",
    "todo",
    "TODO",
    "wikipedia",
    "wikimedia_commons",
    "import_uuid",
    "import",
    "old_name",
    "loc_name",
    "official_name",
    "alt_name",
    "operator",
    "phone",
    "fax",
    "email",
    "url",
    "website",
    "opening_hours",
    "description",
    "attribution",
    "check_date",
    "survey:date",
    "ref:bag",
    "ref:bygningsnr",
];

const IGNORED_PREFIXES: &[&str] = &[
    "addr:",
    "source",
    "name:",
    "alt_name:",
    "contact:",
    "is_in:",
    "operator:",
    "tiger:",
    "NHD:",
    "lacounty:",
    "nysgissam:",
    "ref:ruian:",
    "building:ruian:",
    "osak:",
    "gnis:",
    "yh:",
    "check_date:",
];

/// `start_date` is kept: Arnis picks a facade style from a building's construction year.
/// `addr:housenumber` is kept: it feeds the door plate.
pub fn keep_tag(k: &str) -> bool {
    if k == "addr:housenumber" || k == "start_date" || k == "type" {
        return true;
    }
    !IGNORED_TAGS.contains(&k) && !IGNORED_PREFIXES.iter().any(|p| k.starts_with(p))
}

fn get<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The value exclusions from the Overpass query. Ocean and tidal features are deliberately
/// left to the satellite land-cover pass, which resolves coastlines better than OSM does.
fn excluded_by_value(tags: &[(String, String)]) -> bool {
    if get(tags, "landuse") == Some("salt_pond") {
        return true;
    }
    if matches!(get(tags, "natural"), Some("coastline" | "bay" | "strait")) {
        return true;
    }
    if matches!(get(tags, "water"), Some("bay" | "ocean" | "sea"))
        || get(tags, "tidal") == Some("yes")
    {
        return true;
    }
    if get(tags, "waterway") == Some("tidal_channel") {
        return true;
    }
    if matches!(
        get(tags, "place"),
        Some("ocean" | "sea" | "bay" | "strait" | "sound" | "fjord")
    ) {
        return true;
    }
    false
}

fn has_wanted_key(tags: &[(String, String)]) -> bool {
    tags.iter().any(|(k, _)| WANTED_KEYS.contains(&k.as_str()))
}

pub fn way_is_wanted(tags: &[(String, String)]) -> bool {
    if excluded_by_value(tags) {
        return false;
    }
    // `place` is way-only in the Overpass query: a place node is a label, not geometry.
    has_wanted_key(tags) || get(tags, "place").is_some()
}

pub fn node_is_wanted(tags: &[(String, String)]) -> bool {
    !excluded_by_value(tags) && has_wanted_key(tags)
}

pub fn relation_is_wanted(tags: &[(String, String)]) -> bool {
    if excluded_by_value(tags) {
        return false;
    }
    has_wanted_key(tags) || matches!(get(tags, "type"), Some("multipolygon" | "building"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn keeps_what_arnis_renders_from() {
        assert!(way_is_wanted(&t(&[("building", "house")])));
        assert!(way_is_wanted(&t(&[("highway", "residential")])));
        assert!(node_is_wanted(&t(&[("amenity", "waste_basket")])));
        assert!(relation_is_wanted(&t(&[
            ("type", "multipolygon"),
            ("landuse", "forest")
        ])));
        // An untagged multipolygon member relation still counts: its ways carry the shape.
        assert!(relation_is_wanted(&t(&[("type", "multipolygon")])));
    }

    #[test]
    fn drops_the_ocean_features_land_cover_handles() {
        assert!(!way_is_wanted(&t(&[("natural", "coastline")])));
        assert!(!way_is_wanted(&t(&[("water", "ocean")])));
        assert!(!way_is_wanted(&t(&[("waterway", "tidal_channel")])));
        assert!(!way_is_wanted(&t(&[("landuse", "salt_pond")])));
        assert!(!way_is_wanted(&t(&[
            ("natural", "water"),
            ("tidal", "yes")
        ])));
        // ...but ordinary inland water stays.
        assert!(way_is_wanted(&t(&[("natural", "water")])));
        assert!(way_is_wanted(&t(&[("water", "lake")])));
    }

    #[test]
    fn a_place_label_node_is_not_geometry() {
        assert!(way_is_wanted(&t(&[("place", "square")])));
        assert!(!node_is_wanted(&t(&[("place", "city")])));
    }

    // The tag filter must match osm_parser.rs, including its two deliberate exceptions.
    #[test]
    fn tag_filter_matches_arnis() {
        assert!(!keep_tag("name:de"));
        assert!(!keep_tag("addr:street"));
        assert!(!keep_tag("source"));
        assert!(!keep_tag("website"));
        assert!(keep_tag("addr:housenumber"));
        assert!(keep_tag("start_date"));
        assert!(keep_tag("building:levels"));
        assert!(keep_tag("roof:shape"));
        assert!(keep_tag("height"));
    }
}
